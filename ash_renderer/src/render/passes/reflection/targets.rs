//! 反射目标族（10.3）：每帧槽一张半分辨率反射图 + 一张全分辨率 `hdr_copy`。
//!
//! 尺寸分级同 10.2 的 AO 目标（**resize 级**）：布局与描述符池在 [`ReflectionTargets::new`]
//! 建一次，[`ReflectionTargets::ensure`] 只换 image/view 并重写同一批描述符集。
//!
//! 反射图是 [`REFLECTION_FORMAT`]（RGBA16F）：`rgb` = 命中色（HDR 域，已含曝光），
//! `a` = 权重 w（粗糙度/距离/边缘淡出/强度的乘积）。步进 compute 用 storage 写**半分辨率**
//! 那张，合成 pass 用 `textureLoad` 读——所以这张图必须既能 storage 写又能 sampled 读，
//! 走 [`ColorTarget::new_storage_sampled`]（既有两条构造都给不出这个组合）。
//!
//! `hdr_copy` 是普通颜色目标：拷贝 pass 把合成结果写进去，合成 pass 再读它、写回 HDR——
//! 同一张图不能在一步里既采样又写（反馈环），这就是同帧拷贝的全部理由（主计划 §5 D3）。
//!
//! 关档（[`ReflectionSettings::enabled`] 为 false）时本段整段跳过，所以这里没有 dummy 图：
//! 关档既不解绑集也不进合成 pass 的集序（与 SSAO 的「恒绑 1×1 白」不同，因为 SSR 的
//! 合成是独立 pass 而非主合成的一个分支）。

use ash::{vk, Device};
use bevy::prelude::Resource;

use crate::common::error::Result;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::core::target::color::{ColorTarget, HDR_FORMAT};
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;
const FRAGMENT: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// 反射图格式：HDR 命中色 + 权重需要一个带 alpha 的浮点格式，与 HDR 中间目标同格式。
pub const REFLECTION_FORMAT: vk::Format = HDR_FORMAT;

/// 步进 compute 的写目标集：b0 = 该帧槽的半分辨率反射图。
pub(crate) static REFLECTION_STORAGE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "reflection_storage",
    &[BindingDesc::new(0, DescKind::StorageImage, COMPUTE)],
);

/// 反射合成 pass 的采样集：b0 = 该帧槽的半分辨率反射图，b1 = 该帧槽的 `hdr_copy`。
///
/// 两者都是采样读；放同一个集里是因为它们同寿命、同 resize 时机（一起重建、一起重写描述符）。
pub(crate) static REFLECTION_SAMPLE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "reflection_sample",
    &[
        BindingDesc::new(0, DescKind::SampledImage, FRAGMENT),
        BindingDesc::new(1, DescKind::SampledImage, FRAGMENT),
    ],
);

/// 半分辨率尺寸 = 全分辨率向上取整除以 2（下限 1），与 SSAO 的 `half_extent` 同口径。
pub fn half_extent(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: extent.width.max(1).div_ceil(2),
        height: extent.height.max(1).div_ceil(2),
    }
}

/// 每帧槽的反射图 + `hdr_copy`，以及按帧槽轮转的描述符集。
///
/// 每帧槽一套是必须的：slot 轮转时上一帧的合成可能还在读反射图与 `hdr_copy`。
#[derive(Resource)]
pub struct ReflectionTargets {
    device: Device,
    storage_layout: vk::DescriptorSetLayout,
    storage_pool: vk::DescriptorPool,
    sample_layout: vk::DescriptorSetLayout,
    sample_pool: vk::DescriptorPool,
    half_size: vk::Extent2D,
    full_size: vk::Extent2D,
    half: Vec<ColorTarget>,
    copy: Vec<ColorTarget>,
    storage_sets: Vec<vk::DescriptorSet>,
    sample_sets: Vec<vk::DescriptorSet>,
    transition_pool: vk::CommandPool,
    /// 最近一次被步进写过的帧槽；`None` = 还没写过（`ssr_stats` 不读）。
    written_slot: Option<usize>,
}

impl ReflectionTargets {
    /// 建布局与描述符池（尺寸无关部分）。反射图与 `hdr_copy` 在首次 [`Self::ensure`] 时建。
    ///
    /// # Errors
    /// 布局/池/描述符集创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let device = &ctx.device;
        let mut this = Self {
            device: device.clone(),
            storage_layout: vk::DescriptorSetLayout::null(),
            storage_pool: vk::DescriptorPool::null(),
            sample_layout: vk::DescriptorSetLayout::null(),
            sample_pool: vk::DescriptorPool::null(),
            half_size: vk::Extent2D::default(),
            full_size: vk::Extent2D::default(),
            half: Vec::new(),
            copy: Vec::new(),
            storage_sets: Vec::new(),
            sample_sets: Vec::new(),
            transition_pool: vk::CommandPool::null(),
            written_slot: None,
        };
        this.storage_layout = REFLECTION_STORAGE_SET_DESC.create(device)?;
        let storage_sizes = REFLECTION_STORAGE_SET_DESC.pool_sizes_vec(MAX_FRAMES_IN_FLIGHT as u32);
        this.storage_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&storage_sizes),
                None,
            )?
        };
        let storage_layouts = [this.storage_layout; MAX_FRAMES_IN_FLIGHT];
        this.storage_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.storage_pool)
                    .set_layouts(&storage_layouts),
            )?
        };
        this.sample_layout = REFLECTION_SAMPLE_SET_DESC.create(device)?;
        let sample_sizes = REFLECTION_SAMPLE_SET_DESC.pool_sizes_vec(MAX_FRAMES_IN_FLIGHT as u32);
        this.sample_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&sample_sizes),
                None,
            )?
        };
        let sample_layouts = [this.sample_layout; MAX_FRAMES_IN_FLIGHT];
        this.sample_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.sample_pool)
                    .set_layouts(&sample_layouts),
            )?
        };
        Ok(this)
    }

    /// 尺寸变了就重建反射图与 `hdr_copy` 并重写描述符集；同尺寸空操作。
    ///
    /// # Errors
    /// 图/视图/描述符写或布局迁移失败。已建部分仍归 Self，尺寸保持未完成状态；
    /// 下次 `ensure` 清理后重试，或由退出排空之后的 `Drop` 回收。
    pub fn ensure(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        let full_size = vk::Extent2D {
            width: extent.width.max(1),
            height: extent.height.max(1),
        };
        if self.full_size == full_size && !self.half.is_empty() && !self.copy.is_empty() {
            return Ok(());
        }
        let device = &ctx.device;
        // 换附件前等 GPU 空闲：在飞帧可能正读旧图（resize 已暂停帧循环，不进每帧路径）。
        unsafe { device.device_wait_idle() }?;
        self.drop_transition_pool();
        self.half.clear();
        self.copy.clear();
        self.written_slot = None;
        let half_size = half_extent(full_size);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let half = ColorTarget::new_storage_sampled(
                ctx,
                half_size,
                REFLECTION_FORMAT,
                "SSR 半分辨率反射图",
            )?;
            let copy = ColorTarget::new(ctx, full_size, HDR_FORMAT, false, "SSR HDR 拷贝")?;
            self.half.push(half);
            self.copy.push(copy);
        }
        // 反射图以 UNDEFINED 建好之后一次迁到 GENERAL 并一直留着：步进写、合成读都在
        // GENERAL，省掉每帧布局迁移（同 SSAO 两张 AO 图的做法）。
        let images: Vec<vk::Image> = self.half.iter().map(ColorTarget::image).collect();
        self.transition_from_undefined(ctx, &images)?;
        for slot in 0..MAX_FRAMES_IN_FLIGHT {
            let mut writer = DescriptorWriter::new(&REFLECTION_STORAGE_SET_DESC, self.storage_sets[slot]);
            writer.image(0, self.half[slot].view(), vk::ImageLayout::GENERAL);
            writer.submit(device)?;
            let mut writer =
                DescriptorWriter::new(&REFLECTION_SAMPLE_SET_DESC, self.sample_sets[slot]);
            writer.image(0, self.half[slot].view(), vk::ImageLayout::GENERAL);
            // hdr_copy 由拷贝 pass 每帧写成 COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY，
            // 描述符这里按 SHADER_READ_ONLY 写（帧内屏障负责把布局摆到这个状态）
            writer.image(1, self.copy[slot].view(), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            writer.submit(device)?;
        }
        self.half_size = half_size;
        self.full_size = full_size;
        Ok(())
    }

    /// 一次性把图从 UNDEFINED 迁到 GENERAL（storage/采样都用它）。
    fn transition_from_undefined(&mut self, ctx: &Context, images: &[vk::Image]) -> Result<()> {
        if images.is_empty() {
            return Ok(());
        }
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let device = &ctx.device;
        self.transition_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(ctx.queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )?
        };
        let pool = self.transition_pool;
        let mut submitted = false;
        let result = unsafe {
            (|| -> Result<()> {
                let cbs = device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )?;
                let cb = cbs[0];
                device.begin_command_buffer(
                    cb,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let barriers: Vec<vk::ImageMemoryBarrier> = images
                    .iter()
                    .map(|image| {
                        vk::ImageMemoryBarrier::default()
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::GENERAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(*image)
                            .subresource_range(range)
                            .src_access_mask(vk::AccessFlags::empty())
                            .dst_access_mask(
                                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                            )
                    })
                    .collect();
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
                device.end_command_buffer(cb)?;
                let cbs_to_submit = [cb];
                let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
                device.queue_submit(ctx.queue, &[submit], vk::Fence::null())?;
                submitted = true;
                device.queue_wait_idle(ctx.queue)?;
                Ok(())
            })()
        };
        // 未提交的失败可以立即回收；提交后的等待失败不能假定 GPU 已不再引用池。
        if (!submitted || result.is_ok()) && self.transition_pool != vk::CommandPool::null() {
            unsafe { self.device.destroy_command_pool(self.transition_pool, None) };
            self.transition_pool = vk::CommandPool::null();
        }
        result
    }

    fn drop_transition_pool(&mut self) {
        if self.transition_pool != vk::CommandPool::null() {
            unsafe { self.device.destroy_command_pool(self.transition_pool, None) };
            self.transition_pool = vk::CommandPool::null();
        }
    }

    /// 存储集布局（登记进 `SetLayouts` 给步进管线对账）。
    #[must_use]
    pub fn storage_layout(&self) -> SetLayout {
        SetLayout::new(&REFLECTION_STORAGE_SET_DESC, self.storage_layout)
    }

    /// 采样集布局（登记进 `SetLayouts` 给合成管线对账）。
    #[must_use]
    pub fn sample_layout(&self) -> SetLayout {
        SetLayout::new(&REFLECTION_SAMPLE_SET_DESC, self.sample_layout)
    }

    /// 指定帧槽的步进写目标集。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`（由帧池轮转保证）。
    #[must_use]
    pub fn storage_set(&self, slot: usize) -> DescSet {
        DescSet::new(&REFLECTION_STORAGE_SET_DESC, self.storage_sets[slot])
    }

    /// 指定帧槽的合成采样集。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`（由帧池轮转保证）。
    #[must_use]
    pub fn sample_set(&self, slot: usize) -> DescSet {
        DescSet::new(&REFLECTION_SAMPLE_SET_DESC, self.sample_sets[slot])
    }

    /// 指定帧槽的 `hdr_copy`（拷贝 pass 的颜色附件、合成 pass 的采样源）。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`。
    #[must_use]
    pub(crate) fn copy_target(&self, slot: usize) -> &ColorTarget {
        &self.copy[slot]
    }

    /// 半分辨率尺寸（还没 ensure 过时为零）。
    pub fn half_extent(&self) -> vk::Extent2D {
        self.half_size
    }

    /// 全分辨率尺寸（还没 ensure 过时为零）。
    pub fn full_extent(&self) -> vk::Extent2D {
        self.full_size
    }

    /// 提交成功后登记「这个帧槽的反射图是本帧写的」，供 `ssr_stats` 回读。
    pub fn mark_written(&mut self, slot: usize) {
        self.written_slot = Some(slot);
    }

    /// 最近一次被步进写过的帧槽（`ssr_stats` 的回读目标）。
    #[must_use]
    pub fn written_slot(&self) -> Option<usize> {
        self.written_slot
    }

    /// 指定帧槽的反射图句柄（回读用）。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`，且 `ensure` 已跑过。
    #[must_use]
    pub fn image(&self, slot: usize) -> vk::Image {
        self.half[slot].image()
    }

    #[expect(
        dead_code,
        reason = "关档或 extent 为零时宿主不写、回读不取；保留给 10.3.6 的取证脚本按槽取图"
    )]
    #[must_use]
    pub(crate) fn device(&self) -> &Device {
        &self.device
    }
}

impl Drop for ReflectionTargets {
    fn drop(&mut self) {
        // 池与布局归 Self；图由 ColorTarget 自己的 Drop 回收（不等 GPU，退出排空在此之前）。
        self.drop_transition_pool();
        unsafe {
            if self.storage_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.storage_pool, None);
            }
            if self.sample_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.sample_pool, None);
            }
            if self.storage_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.storage_layout, None);
            }
            if self.sample_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.sample_layout, None);
            }
        }
    }
}
