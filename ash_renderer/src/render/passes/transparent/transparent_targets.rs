//! 透明命令链的 buffer 与描述符集（每帧槽一份，容量固定）。

use ash::{vk, Device};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::error::Result;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::core::binding::storage_sets;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::Context;

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 透明命令集：b0 索引列表 / b1 深度键 / b2 命令环 / b3 计数，只给 compute。
pub(crate) static TRANSPARENT_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "transparent",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, CS),
        BindingDesc::new(1, DescKind::StorageBuffer, CS),
        BindingDesc::new(2, DescKind::StorageBuffer, CS),
        BindingDesc::new(3, DescKind::StorageBuffer, CS),
    ],
);

/// 透明行容量，索引列表、深度键、命令环都按它分配。在场行数超过它时
/// [`super::TransparentPasses::collect`] 直接报错。
pub const TRANSPARENT_CAPACITY: u32 = 16384;

/// 透明命令链，每帧槽一份。容量固定，resize 不重建。set 布局：b0 索引列表 /
/// b1 深度键 / b2 命令环 / b3 计数，只给 compute 用；绘制直接读命令环 buffer。
#[derive(Resource)]
pub struct TransparentTargets {
    device: Device,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    /// 透明行的实例槽号（显存）。
    index_lists: Vec<GpuBuffer>,
    /// 排序用的视深（f32）。
    keys: Vec<GpuBuffer>,
    /// 间接绘制命令，每条 20B。
    commands: Vec<GpuBuffer>,
    /// 透明行数（CPU 可见）：CPU 每帧清零，collect 原子累加，BRP 直读。
    counts: Vec<GpuBuffer>,
}

impl TransparentTargets {
    /// 建到一半失败时，已建的部分由 Drop 回收。
    ///
    /// # Errors
    /// buffer 创建/内存分配/描述符对象创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            index_lists: Vec::new(),
            keys: Vec::new(),
            commands: Vec::new(),
            counts: Vec::new(),
        };
        let sharing = ctx.sharing_families();
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                size,
                role,
                &sharing,
            )
        };
        let capacity = u64::from(TRANSPARENT_CAPACITY);
        let mut slots = Vec::with_capacity(crate::render::MAX_FRAMES_IN_FLIGHT);
        for slot in 0..crate::render::MAX_FRAMES_IN_FLIGHT {
            this.index_lists
                .push(mk(BufferRole::DeviceStorage, capacity * 4)?);
            this.keys.push(mk(BufferRole::DeviceStorage, capacity * 4)?);
            this.commands
                .push(mk(BufferRole::IndirectCommands, capacity * 20)?);
            this.counts.push(mk(BufferRole::Storage, 4)?);
            slots.push([
                this.index_lists[slot].buffer(),
                this.keys[slot].buffer(),
                this.commands[slot].buffer(),
                this.counts[slot].buffer(),
            ]);
        }
        this.set_layout = TRANSPARENT_SET_DESC.create(&ctx.device)?;
        (this.pool, this.sets) = unsafe {
            storage_sets::create_sets(&ctx.device, &TRANSPARENT_SET_DESC, this.set_layout, &slots)
        }?;
        info!(
            "透明命令链就绪: 透明行容量 {TRANSPARENT_CAPACITY}（索引/键列表 + 命令环 + 计数）× {} 帧槽",
            this.sets.len(),
        );
        Ok(this)
    }

    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&TRANSPARENT_SET_DESC, self.set_layout)
    }

    #[must_use]
    pub fn frame_set(&self, slot: usize) -> DescSet {
        DescSet::new(&TRANSPARENT_SET_DESC, self.sets[slot % self.sets.len()])
    }

    #[must_use]
    pub fn command_buffer(&self, slot: usize) -> vk::Buffer {
        self.commands[slot % self.commands.len()].buffer()
    }

    /// 第 `slot` 帧槽的深度键 buffer；每帧先填 -inf，空位排序后落在最前面。
    #[must_use]
    pub fn key_buffer(&self, slot: usize) -> vk::Buffer {
        self.keys[slot % self.keys.len()].buffer()
    }

    /// 第 `slot` 帧槽的透明行数。BRP 读到的是该槽上一轮（两帧前）的结果。
    #[must_use]
    pub fn count(&mut self, slot: usize) -> &mut GpuBuffer {
        let n = self.counts.len();
        &mut self.counts[slot % n]
    }

    /// 读回第 `slot` 帧槽的索引列表和深度键，供 BRP 排序对照。每次临时建
    /// staging，只给低频诊断用，不要放进帧循环。
    ///
    /// # Errors
    /// 读回提交失败（设备丢失等）。
    pub fn readback_sort(
        &self,
        ctx: &Context,
        slot: usize,
        indices: &mut [u8],
        keys: &mut [u8],
    ) -> Result<()> {
        crate::render::core::device::readback::readback_buffer(
            ctx,
            self.index_lists[slot % self.index_lists.len()].buffer(),
            indices,
        )?;
        crate::render::core::device::readback::readback_buffer(
            ctx,
            self.keys[slot % self.keys.len()].buffer(),
            keys,
        )
    }
}

impl Drop for TransparentTargets {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown 已先 device_wait_idle；buffer 随字段释放
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.set_layout, None);
            }
        }
    }
}
