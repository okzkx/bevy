//! overlay 字体图集的 GPU 侧账面：当前代 GpuImage + 槽位、整传换下的旧代
//! 停尸房。生产方是 overlay 的绘制半边（镜像代数领先即整传新代，经
//! [`AtlasGpu::install`] 换代），排空在 driver 的 `flush_uploads` 统一排空点，
//! 残量在 teardown 随表拆除。

use bevy::prelude::*;

use crate::common::error::Result;
use crate::render::{BindlessTextureTable, GpuImage, SlotBinding, Ticket, Uploader};

/// 图集 GPU 侧：当前代资源 + 退役代 graveyard（接统一排空点）。
#[derive(Resource, Default)]
pub struct AtlasGpu {
    /// 当前在用的图集代（最近一次整传）。
    current: Option<AtlasSlot>,
    /// 退役代：整传换下的旧 GpuImage（上传票据随行）——由 flush_uploads 的
    /// 统一排空点在"图形最后使用 + 上传完成"双条件后销毁（否则随 dirty-rect
    /// 代数增长，停尸房与纹理槽无界积聚）。
    graveyard: Vec<(GpuImage, Ticket)>,
    /// 已整传的镜像代数。
    uploaded_generation: u64,
}

/// 一代图集的 GPU 资源与 bindless 槽位。
struct AtlasSlot {
    image: GpuImage,
    /// 本代整传票据（排空双条件之一）。
    ticket: Ticket,
    slots: SlotBinding,
}

impl AtlasGpu {
    /// 已整传的镜像代数（整传判定 + 调试窗口统计行）。
    pub(crate) fn generation(&self) -> u64 {
        self.uploaded_generation
    }

    /// graveyard 累计张数（调试窗口统计行）。
    pub(crate) fn graveyard_len(&self) -> usize {
        self.graveyard.len()
    }

    /// 当前代的槽位绑定（None = 从未整传）。
    pub(crate) fn current_slots(&self) -> Option<SlotBinding> {
        self.current.as_ref().map(|slot| slot.slots)
    }

    /// 新代就位：整传提交成功且槽位发布后调用，已传代数推进到 `generation`。
    /// 旧代退场：本体进停尸房（票据随行，排空点销毁）、槽位交回表。
    pub(crate) fn install(
        &mut self,
        tables: &mut BindlessTextureTable,
        image: GpuImage,
        ticket: Ticket,
        slots: SlotBinding,
        generation: u64,
    ) {
        if let Some(old) = self.current.replace(AtlasSlot { image, ticket, slots }) {
            tables.retire(old.slots);
            self.graveyard.push((old.image, old.ticket));
        }
        self.uploaded_generation = generation;
    }

    /// 排空旧代停尸房：与 ImageCache/MeshPool 同款双条件——
    /// `wait_draws` 等"图形最后使用"（帧槽 fence），`uploader` 等"上传完成"
    ///（旧代整传票据）。空房直接返回不等待（稳态零成本）。
    ///
    /// # Errors
    /// 帧侧等待或票据等待失败（排空未发生，停尸房保留，下帧重试）。
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
            .fold(0u64, u64::max);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        self.graveyard.clear(); // Drop 序：view 先于 image，memory 最后
        info!("图集停尸房排空:销毁 {count} 张旧代（双条件齐备后）");
        Ok(())
    }
}
