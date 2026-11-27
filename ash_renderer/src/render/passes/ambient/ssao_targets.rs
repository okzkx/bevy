//! SSAO 目标族（10.2）：半分辨率 AO + 全分辨率 AO + 关档用的 1×1 白 dummy。
//!
//! 尺寸分四级里的 **resize 级**：布局与描述符池在 [`SsaoTargets::new`] 建一次，
//! [`SsaoTargets::ensure`] 只换 image/view 并重写同一批描述符集（2026-11-24 G-buffer
//! 布局悬空的教训：布局进进程级，resize 只重建附件）。
//!
//! 两张 AO 图都是 [`SSAO_FORMAT`]（R32F）：SSAO pass 用 storage 写半分辨率那张，上采样
//! 写全分辨率那张，合成 pass 与 `ssao_stats` 读全分辨率那张。以 `initialLayout =
//! UNDEFINED` 创建之后一次迁到 `GENERAL` 并一直留着——内容每帧整体重写，省掉每帧布局迁移。
//!
//! 关档（[`SsaoSettings::enabled`](super::SsaoSettings) 为 false）时合成 pass 仍按固定集序
//! 绑一个集：这里给 1×1 白 dummy（值恰为 1.0），AO 乘 1.0 逐字节恒等，管线布局因此恒定。

use ash::{vk, Device};
use bevy::prelude::Resource;

use crate::common::error::Result;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;
const FRAGMENT: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// AO 图的格式：单通道浮点，storage 纹理可写、可 sampled 读、可回读，且无量化。
pub const SSAO_FORMAT: vk::Format = vk::Format::R32_SFLOAT;

/// SSAO compute 的写目标集：b0 = 半分辨率 AO，b1 = 全分辨率 AO（同一帧槽）。
pub(crate) static SSAO_STORAGE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ssao_storage",
    &[
        BindingDesc::new(0, DescKind::StorageImage, COMPUTE),
        BindingDesc::new(1, DescKind::StorageImage, COMPUTE),
    ],
);

/// 合成 pass 的采样集：b0 = 全分辨率 AO（关档绑 dummy）。
pub(crate) static SSAO_SAMPLE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ssao_sample",
    &[BindingDesc::new(0, DescKind::SampledImage, FRAGMENT)],
);

/// 半分辨率尺寸 = 全分辨率向上取整除以 2（下限 1）。
pub fn half_extent(extent: vk::Extent2D) -> vk::Extent2D {
    let full = nonzero_extent(extent);
    vk::Extent2D {
        width: full.width.div_ceil(2),
        height: full.height.div_ceil(2),
    }
}

/// 每帧槽两张 AO 图 + 一张关档 dummy，以及按帧槽轮转的描述符集。
///
/// 每帧槽一套图是必须的：slot 轮转时上一帧的合成可能还在读全分辨率 AO。
#[derive(Resource)]
pub struct SsaoTargets {
    device: Device,
    storage_layout: vk::DescriptorSetLayout,
    storage_pool: vk::DescriptorPool,
    sample_layout: vk::DescriptorSetLayout,
    sample_pool: vk::DescriptorPool,
    half_size: vk::Extent2D,
    full_size: vk::Extent2D,
    half: Vec<SsaoImage>,
    full: Vec<SsaoImage>,
    half_views: Vec<vk::ImageView>,
    full_views: Vec<vk::ImageView>,
    storage_sets: Vec<vk::DescriptorSet>,
    /// 全分辨率诊断模式用的 storage 集：b0 与 b1 都绑全分辨率 AO（写入即全分辨率）。
    full_res_sets: Vec<vk::DescriptorSet>,
    sample_sets: Vec<vk::DescriptorSet>,
    dummy: Option<SsaoImage>,
    dummy_view: vk::ImageView,
    dummy_set: vk::DescriptorSet,
    transition_pool: vk::CommandPool,
    /// 最近一次被 SSAO pass 写过的帧槽；`None` = 还没写过（`ssao_stats` 不读）。
    written_slot: Option<usize>,
}

impl SsaoTargets {
    /// 建布局与描述符池（尺寸无关部分），并建好 1×1 白 dummy。AO 图在首次
    /// [`Self::ensure`] 时建。
    ///
    /// # Errors
    /// 布局/池/描述符集/图像创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let device = &ctx.device;
        // 取得的句柄立即归 Self：任意创建步骤失败，都由同一个 Drop 回收已建部分。
        let mut this = Self {
            device: device.clone(),
            storage_layout: vk::DescriptorSetLayout::null(),
            storage_pool: vk::DescriptorPool::null(),
            sample_layout: vk::DescriptorSetLayout::null(),
            sample_pool: vk::DescriptorPool::null(),
            half_size: vk::Extent2D::default(),
            full_size: vk::Extent2D::default(),
            half: Vec::new(),
            full: Vec::new(),
            half_views: Vec::new(),
            full_views: Vec::new(),
            storage_sets: Vec::new(),
            full_res_sets: Vec::new(),
            sample_sets: Vec::new(),
            dummy: None,
            dummy_view: vk::ImageView::null(),
            dummy_set: vk::DescriptorSet::null(),
            transition_pool: vk::CommandPool::null(),
            written_slot: None,
        };
        this.storage_layout = SSAO_STORAGE_SET_DESC.create(device)?;
        let storage_sizes = SSAO_STORAGE_SET_DESC.pool_sizes_vec(MAX_FRAMES_IN_FLIGHT as u32 * 2);
        this.storage_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32 * 2)
                    .pool_sizes(&storage_sizes),
                None,
            )?
        };
        let storage_layouts = [this.storage_layout; MAX_FRAMES_IN_FLIGHT * 2];
        let storage_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.storage_pool)
                    .set_layouts(&storage_layouts),
            )?
        };
        this.storage_sets = storage_sets[..MAX_FRAMES_IN_FLIGHT].to_vec();
        this.full_res_sets = storage_sets[MAX_FRAMES_IN_FLIGHT..].to_vec();
        // 每个帧槽一个采样集，外加一个关档 dummy 用的集。
        this.sample_layout = SSAO_SAMPLE_SET_DESC.create(device)?;
        let sample_count = MAX_FRAMES_IN_FLIGHT as u32 + 1;
        let sample_sizes = SSAO_SAMPLE_SET_DESC.pool_sizes_vec(sample_count);
        this.sample_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(sample_count)
                    .pool_sizes(&sample_sizes),
                None,
            )?
        };
        let sample_layouts = [this.sample_layout; MAX_FRAMES_IN_FLIGHT + 1];
        let sample_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.sample_pool)
                    .set_layouts(&sample_layouts),
            )?
        };
        this.sample_sets = sample_sets[..MAX_FRAMES_IN_FLIGHT].to_vec();
        this.dummy_set = sample_sets[MAX_FRAMES_IN_FLIGHT];
        this.init_dummy(ctx)?;
        Ok(this)
    }

    /// 建 1×1 白 dummy 并绑进关档采样集；之后不再随 resize 变。
    fn init_dummy(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        let extent = vk::Extent2D {
            width: 1,
            height: 1,
        };
        let image = create_image(
            ctx,
            extent,
            SSAO_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        self.dummy_view = create_view(device, image.image, SSAO_FORMAT)?;
        let mut writer = DescriptorWriter::new(&SSAO_SAMPLE_SET_DESC, self.dummy_set);
        writer.image(0, self.dummy_view, vk::ImageLayout::GENERAL);
        writer.submit(device)?;
        // 迁到 GENERAL 并清成 1.0：关档时 AO 乘 1.0，与没接 AO 逐字节一致。
        let raw = image.image;
        self.dummy = Some(image);
        self.transition_from_undefined(ctx, &[raw], Some(raw))
    }

    /// 尺寸变了就重建全部 AO 图并重写描述符集；同尺寸空操作。
    ///
    /// # Errors
    /// 图/视图/描述符写或布局迁移失败。已建部分仍归 Self，尺寸保持未完成状态；
    /// 下次 `ensure` 清理后重试，或由退出排空之后的 `Drop` 回收。
    pub fn ensure(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        let full_size = nonzero_extent(extent);
        if self.full_size == full_size && !self.full.is_empty() {
            return Ok(());
        }
        let device = &ctx.device;
        // 换附件前等 GPU 空闲：在飞帧可能正读旧图（resize 已暂停帧循环，不进每帧路径）。
        unsafe { device.device_wait_idle() }?;
        self.drop_transition_pool();
        self.drop_targets();
        let half_size = half_extent(full_size);
        let usage = vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC;
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            // 图与视图都立即归 Self：后续失败也不遗失裸句柄。
            let half = create_image(ctx, half_size, SSAO_FORMAT, usage)?;
            let half_view = create_view(device, half.image, SSAO_FORMAT)?;
            self.half.push(half);
            self.half_views.push(half_view);
            let full = create_image(ctx, full_size, SSAO_FORMAT, usage)?;
            let full_view = create_view(device, full.image, SSAO_FORMAT)?;
            self.full.push(full);
            self.full_views.push(full_view);
        }
        for slot in 0..MAX_FRAMES_IN_FLIGHT {
            let mut writer =
                DescriptorWriter::new(&SSAO_STORAGE_SET_DESC, self.storage_sets[slot]);
            writer
                .image(0, self.half_views[slot], vk::ImageLayout::GENERAL)
                .image(1, self.full_views[slot], vk::ImageLayout::GENERAL);
            writer.submit(device)?;
            let mut full_res =
                DescriptorWriter::new(&SSAO_STORAGE_SET_DESC, self.full_res_sets[slot]);
            full_res
                .image(0, self.full_views[slot], vk::ImageLayout::GENERAL)
                .image(1, self.full_views[slot], vk::ImageLayout::GENERAL);
            full_res.submit(device)?;
            let mut sample = DescriptorWriter::new(&SSAO_SAMPLE_SET_DESC, self.sample_sets[slot]);
            sample.image(0, self.full_views[slot], vk::ImageLayout::GENERAL);
            sample.submit(device)?;
        }
        let images: Vec<vk::Image> = self
            .half
            .iter()
            .chain(self.full.iter())
            .map(|image| image.image)
            .collect();
        self.transition_from_undefined(ctx, &images, None)?;
        self.half_size = half_size;
        self.full_size = full_size;
        Ok(())
    }

    /// 一次性把图从 UNDEFINED 迁到 GENERAL；`clear` 给定时顺带清成 1.0。
    ///
    /// 放在 `ensure`/`new` 里而不是每帧录屏障：合成 pass 每帧都绑全分辨率 AO，而 SSAO
    /// 关掉的帧不录本段，那类帧也得有合法布局。resize 本来就让帧循环暂停重建，这里等
    /// 空闲不进入每帧路径。
    fn transition_from_undefined(
        &mut self,
        ctx: &Context,
        images: &[vk::Image],
        clear: Option<vk::Image>,
    ) -> Result<()> {
        if images.is_empty() {
            return Ok(());
        }
        let range = subresource_range();
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
                        let dst_access = if clear == Some(*image) {
                            vk::AccessFlags::TRANSFER_WRITE
                        } else {
                            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE
                        };
                        vk::ImageMemoryBarrier::default()
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::GENERAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(*image)
                            .subresource_range(range)
                            .src_access_mask(vk::AccessFlags::empty())
                            .dst_access_mask(dst_access)
                    })
                    .collect();
                let dst_stage = if clear.is_some() {
                    vk::PipelineStageFlags::TRANSFER
                } else {
                    vk::PipelineStageFlags::ALL_COMMANDS
                };
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    dst_stage,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
                if let Some(dummy) = clear {
                    let value = vk::ClearColorValue {
                        float32: [1.0, 0.0, 0.0, 0.0],
                    };
                    device.cmd_clear_color_image(
                        cb,
                        dummy,
                        vk::ImageLayout::GENERAL,
                        &value,
                        &[range],
                    );
                    // 清完对后续片元采样可见（合成 pass 在别的提交里读）。
                    let after = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(dummy)
                        .subresource_range(range)
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ);
                    device.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[after],
                    );
                }
                device.end_command_buffer(cb)?;
                let cbs_to_submit = [cb];
                let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
                device.queue_submit(ctx.queue, &[submit], vk::Fence::null())?;
                submitted = true;
                device.queue_wait_idle(ctx.queue)?;
                Ok(())
            })()
        };
        // 未提交的失败可以立即回收；提交后的等待失败不能假定 GPU 已不再引用池与附件。
        if transition_pool_can_drop(submitted, result.is_ok()) {
            self.drop_transition_pool();
        }
        result
    }

    fn drop_transition_pool(&mut self) {
        if self.transition_pool != vk::CommandPool::null() {
            unsafe { self.device.destroy_command_pool(self.transition_pool, None) };
            self.transition_pool = vk::CommandPool::null();
        }
    }

    fn drop_targets(&mut self) {
        for view in self
            .half_views
            .drain(..)
            .chain(self.full_views.drain(..))
        {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        self.half.clear();
        self.full.clear();
        self.half_size = vk::Extent2D::default();
        self.full_size = vk::Extent2D::default();
        self.written_slot = None;
    }

    /// 存储集布局（登记进 `SetLayouts` 给 SSAO/上采样管线对账）。
    #[must_use]
    pub fn storage_layout(&self) -> SetLayout {
        SetLayout::new(&SSAO_STORAGE_SET_DESC, self.storage_layout)
    }

    /// 采样集布局（登记进 `SetLayouts` 给合成管线对账）。
    #[must_use]
    pub fn sample_layout(&self) -> SetLayout {
        SetLayout::new(&SSAO_SAMPLE_SET_DESC, self.sample_layout)
    }

    /// 指定帧槽的 SSAO compute 写目标集（b0 半分辨率、b1 全分辨率）。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`（由帧池轮转保证）。
    #[must_use]
    pub fn storage_set(&self, slot: usize) -> DescSet {
        DescSet::new(&SSAO_STORAGE_SET_DESC, self.storage_sets[slot])
    }

    /// 指定帧槽的合成采样集（b0 = 该槽全分辨率 AO）。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`（由帧池轮转保证）。
    #[must_use]
    pub fn sample_set(&self, slot: usize) -> DescSet {
        DescSet::new(&SSAO_SAMPLE_SET_DESC, self.sample_sets[slot])
    }

    /// 全分辨率诊断模式的 storage 集：b0/b1 都绑全分辨率 AO，dispatch 出来的 AO 不经上采样。
    ///
    /// 只给判定线 4（半/全分辨率对照）与读图取证用；正常渲染路径不碰它。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT`。
    #[must_use]
    pub fn full_res_storage_set(&self, slot: usize) -> DescSet {
        DescSet::new(&SSAO_STORAGE_SET_DESC, self.full_res_sets[slot])
    }

    /// 关档时合成 pass 绑的采样集（1×1 白）。
    #[must_use]
    pub fn dummy_sample_set(&self) -> DescSet {
        DescSet::new(&SSAO_SAMPLE_SET_DESC, self.dummy_set)
    }

    /// 半分辨率尺寸（还没 ensure 过时为零）。
    pub fn half_extent(&self) -> vk::Extent2D {
        self.half_size
    }

    /// 全分辨率尺寸（还没 ensure 过时为零）。
    pub fn full_extent(&self) -> vk::Extent2D {
        self.full_size
    }

    /// 指定帧槽的全分辨率 AO 图（`ssao_stats` 回读用）。
    ///
    /// # Panics
    /// `slot` 必须小于 `MAX_FRAMES_IN_FLIGHT` 且已 `ensure`。
    #[must_use]
    pub fn full_image(&self, slot: usize) -> vk::Image {
        self.full[slot].image
    }

    /// 记下刚被 SSAO pass 写过的帧槽（`ssao_stats` 只在写过之后才读）。
    pub fn mark_written(&mut self, slot: usize) {
        self.written_slot = Some(slot);
    }

    /// 最近一次写过 AO 的帧槽；`None` 表示还没写过，不该回读。
    #[must_use]
    pub fn written_slot(&self) -> Option<usize> {
        self.written_slot
    }
}

impl Drop for SsaoTargets {
    fn drop(&mut self) {
        // 退出排空由 teardown 先行；构造失败尚未提交，不存在在飞引用。
        self.drop_transition_pool();
        self.drop_targets();
        if self.dummy_view != vk::ImageView::null() {
            unsafe { self.device.destroy_image_view(self.dummy_view, None) };
        }
        self.dummy = None;
        unsafe {
            if self.storage_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.storage_pool, None);
            }
            if self.storage_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.storage_layout, None);
            }
            if self.sample_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.sample_pool, None);
            }
            if self.sample_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.sample_layout, None);
            }
        }
    }
}

/// 一张 AO 图及其 dedicated memory；销毁顺序固定（image 先于 memory）。
struct SsaoImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for SsaoImage {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn nonzero_extent(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: extent.width.max(1),
        height: extent.height.max(1),
    }
}

fn subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn transition_pool_can_drop(submitted: bool, completed: bool) -> bool {
    !submitted || completed
}

/// 建一张可 storage 写、可 sampled 读的 2D 图（`initialLayout = UNDEFINED`）。
///
/// `vkCreateImage` 只接受 UNDEFINED/PREINITIALIZED：之后由一道显式屏障送到 GENERAL，
/// 并一直留在那里（内容每帧整体重写）。
fn create_image(
    ctx: &Context,
    extent: vk::Extent2D,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Result<SsaoImage> {
    let device = &ctx.device;
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: extent.width.max(1),
                    height: extent.height.max(1),
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }?;
    let memory = match create_bound_memory(ctx, image) {
        Ok(memory) => memory,
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            return Err(e);
        }
    };
    Ok(SsaoImage {
        device: device.clone(),
        image,
        memory,
    })
}

fn create_view(device: &Device, image: vk::Image, format: vk::Format) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(subresource_range());
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

/// image 的 dedicated memory（DEVICE_LOCAL、恰量分配、offset 0）。
fn create_bound_memory(ctx: &Context, image: vk::Image) -> Result<vk::DeviceMemory> {
    unsafe {
        let reqs = ctx.device.get_image_memory_requirements(image);
        let memory_type = ctx.memory_contract().find_type(
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        )?;
        let memory = ctx.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(memory_type),
            None,
        )?;
        if let Err(e) = ctx.device.bind_image_memory(image, memory, 0) {
            ctx.device.free_memory(memory, None);
            return Err(e.into());
        }
        Ok(memory)
    }
}
