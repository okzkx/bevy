//! overlay 字体图集的 GPU 侧：当前代图像与 bindless 槽位，以及整张重传后换下、
//! 等待销毁的旧代（graveyard）。overlay 绘制侧发现 CPU 镜像代数领先就整张重传，
//! 经 [`AtlasGpu::install`] 换代；graveyard 在 driver 的 `flush_uploads` 统一排空，
//! 剩余的在 teardown 随表拆除。

use bevy::prelude::*;

use std::collections::HashMap;

use crate::common::error::Result;
use crate::render::{BindlessTextureTable, GpuImage, SlotBinding, Ticket, Uploader};

#[derive(Resource, Default)]
pub struct AtlasGpu {
    /// 当前在用的图集代（最近一次整传）。
    current: Option<AtlasSlot>,
    /// 用户贴图（egui 用户纹理，键 = Managed 序号；产出方 `overlay::framework::paint`
    /// 负责整传与释放，本表只管 GPU 资源与槽位寿命）。
    user: HashMap<u64, AtlasSlot>,
    /// 换下的旧代及其上传票据，绘制和上传都完成后才销毁（[`Self::drain_graveyard`]）；
    /// 不排空会随重传次数无界增长。
    graveyard: Vec<(GpuImage, Ticket)>,
    /// 已整传的镜像代数。
    uploaded_generation: u64,
}

/// 一代图集的 GPU 资源与 bindless 槽位。
struct AtlasSlot {
    image: GpuImage,
    /// 本代整传票据。
    ticket: Ticket,
    slots: SlotBinding,
}

impl AtlasGpu {
    /// 已整传的镜像代数（整传判定 + 调试窗口统计行）。
    pub(crate) fn generation(&self) -> u64 {
        self.uploaded_generation
    }

    /// graveyard 张数（调试窗口统计行）。
    pub(crate) fn graveyard_len(&self) -> usize {
        self.graveyard.len()
    }

    /// 当前代的槽位绑定（None = 从未整传）。
    pub(crate) fn current_slots(&self) -> Option<SlotBinding> {
        self.current.as_ref().map(|slot| slot.slots)
    }

    /// 用户贴图的槽位绑定（None = 未整传/已释放）。
    pub(crate) fn user_slots(&self, id: u64) -> Option<SlotBinding> {
        self.user.get(&id).map(|slot| slot.slots)
    }

    /// 用户贴图就位：整传提交成功且槽位发布后调用。同 id 重传 = 旧资源进 graveyard。
    pub(crate) fn install_user(
        &mut self,
        tables: &mut BindlessTextureTable,
        id: u64,
        image: GpuImage,
        ticket: Ticket,
        slots: SlotBinding,
    ) {
        if let Some(old) = self.user.insert(
            id,
            AtlasSlot {
                image,
                ticket,
                slots,
            },
        ) {
            tables.retire(old.slots);
            self.graveyard.push((old.image, old.ticket));
        }
    }

    /// 释放用户贴图（产出方收到 egui free 增量时调用）：槽位交回表，GPU 资源
    /// 连同票据进 graveyard 等统一排空。未登记的 id 静默跳过（幂等）。
    pub(crate) fn free_user(&mut self, tables: &mut BindlessTextureTable, id: u64) {
        if let Some(slot) = self.user.remove(&id) {
            tables.retire(slot.slots);
            self.graveyard.push((slot.image, slot.ticket));
        }
    }

    /// 新代就位：整传提交成功且槽位发布后调用，已传代数推进到 `generation`。
    /// 旧代进 graveyard，槽位交回表。
    pub(crate) fn install(
        &mut self,
        tables: &mut BindlessTextureTable,
        image: GpuImage,
        ticket: Ticket,
        slots: SlotBinding,
        generation: u64,
    ) {
        if let Some(old) = self.current.replace(AtlasSlot {
            image,
            ticket,
            slots,
        }) {
            tables.retire(old.slots);
            self.graveyard.push((old.image, old.ticket));
        }
        self.uploaded_generation = generation;
    }

    /// 排空 graveyard：先 `wait_draws` 等在飞帧用完（帧槽 fence），再等旧代里最大的
    /// 上传票据完成，然后销毁。为空时直接返回、不等待。
    ///
    /// # Errors
    /// 帧侧等待或票据等待失败（graveyard 保留，下帧重试）。
    pub(crate) fn drain_graveyard(
        &mut self,
        uploader: &Uploader,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        let max_ticket = self
            .graveyard
            .iter()
            .map(|(_, ticket)| *ticket)
            .max()
            .unwrap_or(0);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        self.graveyard.clear();
        info!("图集待回收队列排空:销毁 {count} 张旧代（双条件齐备后）");
        Ok(())
    }
}
