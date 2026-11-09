//! Hi-Z 深度金字塔：上一轮深度逐级 MIN 归约成的层级深度图，遮挡测试从这里取数。
//! R32_SFLOAT ×2 帧槽，每帧由 compute 链全量重写。
//!
//! 帧内录制序：视锥剔除 → 遮挡测试（读同槽金字塔 = 两帧前本帧槽的归约输出，
//! `wait_for_slot` 的 fence 保证其提交已完成）→ 渲染写深度 → 归约链逐级重写本槽金字塔。
//! "遮挡读旧 → 归约写新"在同一条命令缓冲内，由屏障收口。深度附件渲染后转
//! DEPTH_READ_ONLY 供 level 1 归约采样，下帧 UNDEFINED 进场丢弃，金字塔才是跨帧持久层。
//!
//! 描述符：归约走独立小布局（b0 源采样 / b1 目标 storage，每 (帧槽,级) 一套 set），
//! 遮挡走另一个独立小布局（b0 = 全链采样视图）。不进 set2：金字塔随 swapchain 重建
//! 换 image/视图，而 set2 只在 init 写一次。resize 时 [`HizPyramid::rebuild`] 重建
//! image/视图、reset pool 并重写描述符，管线与 pool 不随 extent 变（pool 按设备最大
//! 级数预留）。
//!
//! 重建后两个帧槽的金字塔内容都未定义，前 2 帧遮挡测试整段跳过（`ready()`）。

use ash::{vk, Device};
use bevy::log::info;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::common::error::{Result, VulkanError};
use crate::common::syntax::unwrap_or_return;

use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts,
};
use crate::render::core::binding::shader_abi;
use crate::render::core::device::readback::OneShotReadback;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::Handoff;
use crate::render::resident::assets::image_spec::full_mip_chain;
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 归约一级：b0 上一级（采样）/ b1 本级（存储写）。
pub(crate) static HIZ_REDUCE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "hiz_reduce",
    &[
        BindingDesc::new(0, DescKind::SampledImage, CS),
        BindingDesc::new(1, DescKind::StorageImage, CS),
    ],
);

/// 遮挡测试读整条金字塔：b0 全 mip 采样视图。
pub(crate) static HIZ_OCCLUSION_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "hiz_occlusion",
    &[BindingDesc::new(0, DescKind::SampledImage, CS)],
);

pub(crate) static HIZ_DOWNSAMPLE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "hiz_downsample",
    shader: &shader_abi::HIZ_DOWNSAMPLE,
    entries: &[c"cs_main"],
    sets: &[&HIZ_REDUCE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 金字塔格式：深度值域 `[0,1]` 直存。能力不足在 [`HizPyramid::new`] 报错退出，不静默换格式。
const HIZ_FORMAT: vk::Format = vk::Format::R32_SFLOAT;

// ===== CPU 参照纯函数（与 hiz_downsample.wgsl / occlusion.wgsl 同语义，单测钉方向）=====

/// reverse-Z tile 归约取 MIN。**方向钉死**：本仓深度 reverse-Z（近=1、远=0），值越小
/// = 越远；tile 的保守遮挡上界 = tile 内最远的已缓冲表面 = 值最小者。正向 Z 的同类
/// 归约取 MAX，方向相反。
#[must_use]
pub fn reduce_tile_reverse_z(a: f32, b: f32, c: f32, d: f32) -> f32 {
    a.min(b).min(c).min(d)
}

/// reverse-Z 遮挡判定：物体"最近角点"的深度值（上界，见 occlusion.wgsl 头注）小于
/// 覆盖区域的最远缓冲表面（tile MIN）→ 全身被挡。**等号 = 可见**（宁多画不错杀）。
#[must_use]
pub fn occluded_reverse_z(obj_closest_ndc: f32, tile_min: f32) -> bool {
    obj_closest_ndc < tile_min
}

/// mip 层选择：`ceil(log2(AABB 像素最大边))`。该层 tile 边长 ≥ AABB/2，AABB 每轴至多
/// 横跨 2 tile，4 tap 的 2×2 邻域必覆盖。**下限 1**（level 0 不派发、无内容，见
/// [`HizPyramid::record_reduction`]），clamp 进级数。
#[must_use]
pub fn occlusion_mip_level(px_w: f32, px_h: f32, levels: u32) -> u32 {
    let max_dim = px_w.max(px_h).max(1.0);
    let wanted = bevy::math::ops::log2(max_dim).ceil() as u32;
    if levels <= 1 {
        return 0;
    }
    wanted.clamp(1, levels - 1)
}

// ===== 资源本体 =====

/// 一帧槽的金字塔 image + dedicated memory（DEVICE_LOCAL）。
struct HizImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for HizImage {
    fn drop(&mut self) {
        // 不等 GPU：重建路径的等待在 rebuild，退出排空在 teardown_vulkan（先于本结构 Drop）。
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 单帧槽的全部视图。
struct HizViews {
    device: Device,
    /// 遮挡测试的全链采样视图（GENERAL 布局，textureLoad 按层取）。
    occlusion: vk::ImageView,
    /// 归约目标 storage 视图（每级一张）。
    storage: Vec<vk::ImageView>,
    /// 归约源采样视图（每级一张）：`[0]`、`[1]` = 该帧槽的深度附件（`[0]` 不派发，只占位），
    /// [L≥2] = 金字塔 L-1 级。
    src: Vec<vk::ImageView>,
}

impl Drop for HizViews {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image_view(self.occlusion, None);
            for view in self.storage.drain(..).chain(self.src.drain(..)) {
                self.device.destroy_image_view(view, None);
            }
        }
    }
}

/// 归约 compute 管线 + 描述符：管线/布局/pool 不依赖 extent，`new` 建一次跨重建复用；
/// set 内容随 init/rebuild 重写。句柄 null / None = 未建成。
struct ReductionPass {
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    pipeline: Option<ComputePipeline>,
    /// pool 预留的级数上限（设备 maxImageDimension2D 的完整 mip 链）。
    /// 坑：按首建级数定死，窗口放大、级数变多时会 OUT_OF_POOL_MEMORY。
    pool_levels: u32,
    /// set 下标 = slot × levels + level。
    sets: Vec<vk::DescriptorSet>,
}

/// Hi-Z 深度金字塔：×2 帧槽 image + 全部视图 + 归约管线 + 遮挡测试的全链采样描述符。
#[derive(bevy::prelude::Resource)]
pub struct HizPyramid {
    device: Device,
    slots: Vec<HizImage>,
    views: Vec<HizViews>,
    /// 每帧槽的金字塔已初始化旗标：false → 首次归约走 UNDEFINED→GENERAL，之后恒
    /// GENERAL。用 AtomicBool 只因 record_reduction 以 &self 录制（单线程，无竞争）。
    initialized: Vec<AtomicBool>,
    /// 重建后的预热计数（>0 = 内容未就绪，遮挡测试跳过；每次归约减一）。
    warmup: u32,
    levels: u32,
    extent: vk::Extent2D,
    reduction: ReductionPass,
    /// 遮挡测试的全链采样描述符（独立小布局，每帧槽一套）。
    occlusion_layout: vk::DescriptorSetLayout,
    occlusion_pool: vk::DescriptorPool,
    occlusion_sets: Vec<vk::DescriptorSet>,
}

/// 一次读回的统计行（只进计量/验证，不进绘制路径）。
#[derive(Debug)]
pub struct HizCensus {
    pub level: u32,
    pub width: u32,
    pub height: u32,
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    /// 恰为 0 的像素占比（背景/远平面 clear 值——占比异常 = 归约链路故障信号）。
    pub zero_ratio: f32,
}

impl HizPyramid {
    /// 读回指定级全量（原始 f32 行主序）。布局 GENERAL → TRANSFER_SRC_OPTIMAL →
    /// 回 GENERAL。
    ///
    /// # Errors
    /// staging/命令池/fence 创建、录制、提交或映射失败。
    pub fn readback_level_raw(
        &self,
        ctx: &Context,
        slot: usize,
        level: u32,
    ) -> Result<(u32, u32, Vec<f32>)> {
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as S};
        let (w, h) = self.level_dims(level);
        let image = self.slots[slot % self.slots.len()].image;
        let byte_size = u64::from(w) * u64::from(h) * 4;
        let range = color_range(level, 1);
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(level)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            });
        let mut bytes = vec![0u8; byte_size as usize];
        unsafe {
            let readback = OneShotReadback::begin(ctx, byte_size)?;
            // 进场 src 取 ALL_COMMANDS/MEMORY_WRITE 宽口径：写者是前序提交里的归约
            // compute，提交序把它们纳入第一同步域
            pyramid_barrier(
                &ctx.device,
                readback.cb,
                image,
                range,
                (L::GENERAL, L::TRANSFER_SRC_OPTIMAL),
                (S::ALL_COMMANDS, S::TRANSFER),
                (A::MEMORY_WRITE, A::TRANSFER_READ),
            );
            ctx.device.cmd_copy_image_to_buffer(
                readback.cb,
                image,
                L::TRANSFER_SRC_OPTIMAL,
                readback.staging,
                &[region],
            );
            pyramid_barrier(
                &ctx.device,
                readback.cb,
                image,
                range,
                (L::TRANSFER_SRC_OPTIMAL, L::GENERAL),
                (S::TRANSFER, S::COMPUTE_SHADER),
                (A::TRANSFER_READ, A::SHADER_READ | A::SHADER_WRITE),
            );
            readback.finish_and_read(ctx, &mut bytes)?;
        }
        let floats = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_ne_bytes(*c))
            .collect();
        Ok((w, h, floats))
    }

    /// 读回指定级并出统计（语义见 [`HizCensus`]）。
    ///
    /// # Errors
    /// 同 [`HizPyramid::readback_level_raw`]。
    pub fn readback_level(&self, ctx: &Context, slot: usize, level: u32) -> Result<HizCensus> {
        let (w, h, floats) = self.readback_level_raw(ctx, slot, level)?;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut zeros = 0u64;
        for &v in &floats {
            min = min.min(v);
            max = max.max(v);
            sum += f64::from(v);
            if v == 0.0 {
                zeros += 1;
            }
        }
        Ok(HizCensus {
            level,
            width: w,
            height: h,
            min,
            max,
            mean: (sum / floats.len() as f64) as f32,
            zero_ratio: zeros as f32 / floats.len() as f32,
        })
    }

    /// 建金字塔：查格式能力 → ×2 帧槽 image → 全部视图 → 归约管线与描述符 → 遮挡测试
    /// 描述符。`depth_images` = 各帧槽深度 image（level 1 的归约源）。
    ///
    /// # Errors
    /// 格式能力不足，或 Vulkan 创建/分配/绑定失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D, depth_images: &[vk::Image]) -> Result<Self> {
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, HIZ_FORMAT)
        }
        .optimal_tiling_features;
        // 三用途：compute 归约写、遮挡/逐级读、CPU 读回拷贝
        let required = vk::FormatFeatureFlags::STORAGE_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::TRANSFER_SRC;
        if !features.contains(required) {
            return Err(VulkanError::Init(format!(
                "Hi-Z 格式 {HIZ_FORMAT:?} 的 optimal tiling features {features:?} 不含所需 {required:?}，金字塔无法按该使用角色建立"
            )));
        }
        let levels = full_mip_chain(extent.width, extent.height);
        info!(
            "Hi-Z 格式能力实测: {HIZ_FORMAT:?} optimal features 含 STORAGE_IMAGE | SAMPLED_IMAGE | TRANSFER_SRC（{levels} 级）"
        );
        let mut this = Self {
            device: ctx.device.clone(),
            slots: Vec::new(),
            views: Vec::new(),
            initialized: (0..MAX_FRAMES_IN_FLIGHT)
                .map(|_| AtomicBool::new(false))
                .collect(),
            warmup: MAX_FRAMES_IN_FLIGHT as u32,
            levels,
            extent,
            reduction: ReductionPass {
                set_layout: vk::DescriptorSetLayout::null(),
                pool: vk::DescriptorPool::null(),
                pipeline: None,
                pool_levels: 0,
                sets: Vec::new(),
            },
            occlusion_layout: vk::DescriptorSetLayout::null(),
            occlusion_pool: vk::DescriptorPool::null(),
            occlusion_sets: Vec::new(),
        };
        this.build_images_and_views(ctx, depth_images)?;
        this.build_reduction_pipeline(ctx)?;
        this.write_reduction_sets(ctx)?;
        this.build_occlusion_sets(ctx)?;
        info!(
            "Hi-Z 金字塔就绪: {HIZ_FORMAT:?} {}x{} ×{levels} 级 ×{MAX} 帧槽（每帧渲染后逐级归约重写；\
             遮挡测试读同槽两帧前输出——fence 纪律零新同步形状；重建 warmup {MAX} 帧）",
            extent.width,
            extent.height,
            MAX = MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 金字塔 mip 级数。
    #[must_use]
    pub fn levels(&self) -> u32 {
        self.levels
    }

    /// 金字塔内容是否就绪（重建后两个帧槽都归约过一次）。
    #[must_use]
    pub fn ready(&self) -> bool {
        self.warmup == 0
    }

    /// 第 `slot` 帧槽的遮挡测试描述符集。
    #[must_use]
    pub fn occlusion_set(&self, slot: usize) -> DescSet {
        DescSet::new(&HIZ_OCCLUSION_SET_DESC, self.occlusion_sets[slot])
    }

    /// 遮挡测试的描述符集布局（建遮挡管线 layout 的素材）。
    #[must_use]
    pub fn occlusion_layout(&self) -> SetLayout {
        SetLayout::new(&HIZ_OCCLUSION_SET_DESC, self.occlusion_layout)
    }

    /// 一次归约链录制完成后的走账：warmup 递减。
    pub fn note_reduced(&mut self) {
        self.warmup = self.warmup.saturating_sub(1);
    }

    /// resize 随 swapchain 重建（extent 没变则跳过）。销毁旧资源前自带
    /// device_wait_idle（在飞命令可能还引用旧 image/视图）；warmup 重置，遮挡测试
    /// 跳过到两个帧槽各归约一次之后。
    ///
    /// # Errors
    /// 等待排空或新资源创建失败。
    pub fn rebuild(
        &mut self,
        ctx: &Context,
        extent: vk::Extent2D,
        depth_images: &[vk::Image],
    ) -> Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        self.drop_resources();
        self.extent = extent;
        self.levels = full_mip_chain(extent.width, extent.height);
        self.initialized = (0..MAX_FRAMES_IN_FLIGHT)
            .map(|_| AtomicBool::new(false))
            .collect();
        self.warmup = MAX_FRAMES_IN_FLIGHT as u32;
        self.build_images_and_views(ctx, depth_images)?;
        self.write_reduction_sets(ctx)?;
        self.build_occlusion_sets(ctx)?;
        info!(
            "Hi-Z 金字塔随 swapchain 重建: {}x{} ×{} 级（{} 帧槽，warmup 重置，遮挡测试随后就绪）",
            extent.width, extent.height, self.levels, MAX_FRAMES_IN_FLIGHT,
        );
        Ok(())
    }

    /// 录制一帧的归约链（`record_frame` 在渲染后调用，调用方已把深度转 DEPTH_READ_ONLY）：
    /// 进场屏障 → 每级绑 (slot, level) set、dispatch → 级间内存屏障。
    pub(crate) fn record_reduction(&self, device: &Device, cb: vk::CommandBuffer, slot: usize) {
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as S};
        // new 成功即 Some；None 只出现在 new 半建失败后的 Drop 路径，录制不可达
        let reduction = unwrap_or_return!(self.reduction.pipeline.as_ref()).handles();
        let slot = slot % self.slots.len();
        unsafe {
            // 进场：首用 UNDEFINED→GENERAL；之后恒 GENERAL，只让归约写排在本帧
            // 更早的遮挡读之后
            let (old_layout, src_stage, src_access) =
                if self.initialized[slot].swap(true, Ordering::Relaxed) {
                    (L::GENERAL, S::COMPUTE_SHADER, A::SHADER_READ)
                } else {
                    (L::UNDEFINED, S::TOP_OF_PIPE, A::empty())
                };
            pyramid_barrier(
                device,
                cb,
                self.slots[slot].image,
                color_range(0, self.levels),
                (old_layout, L::GENERAL),
                (src_stage, S::COMPUTE_SHADER),
                (src_access, A::SHADER_WRITE),
            );
            // 坑：从 level 1 起派发（level 1 = 深度 2×2 MIN），level 0 不派发、无内容。
            // 把 2×2 MIN 写进全尺寸 level 0 只写到左上 1/4，其余未定义，整条链读垃圾
            // （实测高层 tap 读 0、遮挡恒可见）。遮挡侧 mip 选择因此下限 1。
            for level in 1..self.levels {
                let set = self.reduction.sets[slot * self.levels as usize + level as usize];
                reduction.bind(device, cb, &[DescSet::new(&HIZ_REDUCE_SET_DESC, set)]);
                let (w, h) = self.level_dims(level);
                device.cmd_dispatch(cb, w.div_ceil(8), h.div_ceil(8), 1); // 8 = workgroup 边长
                                                                          // 级间屏障：本级写 → 下级 textureLoad。末级没有这条：它的写 → 下一轮
                                                                          // 遮挡读由下一轮 cull 的全局内存屏障罩住（遮挡只在 cull 开时派发）
                if level + 1 < self.levels {
                    Handoff::ComputeRead.barrier().record(device, cb);
                }
            }
        }
    }

    /// 第 `level` 级的尺寸（逐级减半取整，下限 1）。
    #[must_use]
    pub fn level_dims(&self, level: u32) -> (u32, u32) {
        (
            (self.extent.width >> level).max(1),
            (self.extent.height >> level).max(1),
        )
    }

    /// image + 内存 + 视图 ×2 帧槽，重建路径复用（drop_resources 已先清）。任一步失败，
    /// 已建部分随局部变量 Drop。
    fn build_images_and_views(&mut self, ctx: &Context, depth_images: &[vk::Image]) -> Result<()> {
        let device = &ctx.device;
        let mut slots = Vec::new();
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let image = unsafe {
                device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(HIZ_FORMAT)
                        .extent(vk::Extent3D {
                            width: self.extent.width,
                            height: self.extent.height,
                            depth: 1,
                        })
                        .mip_levels(self.levels)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(
                            vk::ImageUsageFlags::STORAGE
                                | vk::ImageUsageFlags::SAMPLED
                                | vk::ImageUsageFlags::TRANSFER_SRC,
                        )
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
            }?;
            let memory = unsafe { create_bound_memory(ctx, image) }
                .inspect_err(|_| unsafe { device.destroy_image(image, None) })?;
            slots.push(HizImage {
                device: device.clone(),
                image,
                memory,
            });
        }
        self.slots = slots;
        let mut views = Vec::with_capacity(self.slots.len());
        for (slot, hiz_image) in self.slots.iter().enumerate() {
            let image = hiz_image.image;
            let mut current = HizViews {
                device: device.clone(),
                occlusion: unsafe {
                    create_view(device, image, HIZ_FORMAT, color_range(0, self.levels))
                }?,
                storage: Vec::with_capacity(self.levels as usize),
                src: Vec::with_capacity(self.levels as usize),
            };
            for level in 0..self.levels {
                current.storage.push(unsafe {
                    create_view(device, image, HIZ_FORMAT, color_range(level, 1))
                }?);
                // level 0/1 的源 = 深度附件（D32，DEPTH aspect），L≥2 = 上一级
                let source = if level <= 1 {
                    let depth = vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(1);
                    unsafe {
                        create_view(
                            device,
                            depth_images[slot],
                            crate::render::DEPTH_FORMAT,
                            depth,
                        )
                    }
                } else {
                    unsafe { create_view(device, image, HIZ_FORMAT, color_range(level - 1, 1)) }
                };
                current.src.push(source?);
            }
            views.push(current);
        }
        self.views = views;
        Ok(())
    }

    /// 归约管线 + set layout + 描述符 pool（`new` 建一次，跨重建复用）。每建成一个句柄
    /// 即落进 `self.reduction`，中途失败由 Drop 回收已建部分。pool 按设备最大 mip 链
    /// 预留，任意合法 extent 都装得下，rebuild 只 reset 不重建。
    fn build_reduction_pipeline(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        self.reduction.set_layout = HIZ_REDUCE_SET_DESC.create(device)?;
        let layouts = SetLayouts::new([SetLayout::new(
            &HIZ_REDUCE_SET_DESC,
            self.reduction.set_layout,
        )])?;
        self.reduction.pipeline = Some(ComputePipeline::new(
            ctx,
            &HIZ_DOWNSAMPLE_PIPELINE,
            &layouts,
        )?);
        let max_dim = ctx.limits().max_image_dimension2_d;
        self.reduction.pool_levels = full_mip_chain(max_dim, max_dim);
        self.reduction.pool = create_pool(
            device,
            &HIZ_REDUCE_SET_DESC,
            MAX_FRAMES_IN_FLIGHT as u32 * self.reduction.pool_levels,
        )?;
        Ok(())
    }

    /// 归约描述符：reset pool → 每 (帧槽,级) 分配一套 set → 写入当前视图（首建与
    /// rebuild 共用；调用前视图已就位）。
    fn write_reduction_sets(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        if self.levels > self.reduction.pool_levels {
            return Err(VulkanError::Init(format!(
                "Hi-Z {} 级超出描述符 pool 预留的 {} 级（extent 超过设备 maxImageDimension2D）",
                self.levels, self.reduction.pool_levels
            )));
        }
        self.reduction.sets.clear();
        let sets = realloc_sets(
            device,
            self.reduction.pool,
            self.reduction.set_layout,
            MAX_FRAMES_IN_FLIGHT * self.levels as usize,
        )?;
        // 声明布局须与 image 实际布局一致（VUID-00344）：level 0/1 的源 = 深度附件
        //（DEPTH_READ_ONLY，record.rs 帧尾屏障落位；[0] 不派发），其余 = 金字塔（恒 GENERAL）
        for slot in 0..MAX_FRAMES_IN_FLIGHT {
            for level in 0..self.levels as usize {
                let set = sets[slot * self.levels as usize + level];
                let mut writer = DescriptorWriter::new(&HIZ_REDUCE_SET_DESC, set);
                writer
                    .image(
                        0,
                        self.views[slot].src[level],
                        if level <= 1 {
                            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL
                        } else {
                            vk::ImageLayout::GENERAL
                        },
                    )
                    .image(1, self.views[slot].storage[level], vk::ImageLayout::GENERAL);
                writer.submit(device)?;
            }
        }
        self.reduction.sets = sets;
        Ok(())
    }

    /// 遮挡测试的全链采样描述符（首建建 layout + pool，重建只重分 set）。
    fn build_occlusion_sets(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        if self.occlusion_pool == vk::DescriptorPool::null() {
            self.occlusion_layout = HIZ_OCCLUSION_SET_DESC.create(device)?;
            self.occlusion_pool =
                create_pool(device, &HIZ_OCCLUSION_SET_DESC, MAX_FRAMES_IN_FLIGHT as u32)?;
        }
        self.occlusion_sets = realloc_sets(
            device,
            self.occlusion_pool,
            self.occlusion_layout,
            MAX_FRAMES_IN_FLIGHT,
        )?;
        for (set, views) in self.occlusion_sets.iter().zip(&self.views) {
            let mut writer = DescriptorWriter::new(&HIZ_OCCLUSION_SET_DESC, *set);
            writer.image(0, views.occlusion, vk::ImageLayout::GENERAL);
            writer.submit(device)?;
        }
        Ok(())
    }

    /// 拆掉每次重建都换的部分：视图先于 image。pipeline/layout/pool 跨重建复用，
    /// 随 Self Drop 终拆。
    fn drop_resources(&mut self) {
        self.views.clear();
        self.slots.clear();
    }
}

impl Drop for HizPyramid {
    fn drop(&mut self) {
        self.drop_resources();
        self.reduction.pipeline = None;
        // new 中途失败时部分句柄是 null，destroy null 是规范允许的 no-op
        unsafe {
            self.device
                .destroy_descriptor_set_layout(self.reduction.set_layout, None);
            self.device
                .destroy_descriptor_pool(self.reduction.pool, None);
            self.device
                .destroy_descriptor_pool(self.occlusion_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.occlusion_layout, None);
        }
    }
}

/// `first_level` 起 `level_count` 级的 subresource range（R32_SFLOAT 是颜色格式）。
fn color_range(first_level: u32, level_count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(first_level)
        .level_count(level_count)
        .layer_count(1)
}

/// 单布局 pool：恰好容纳 `max_sets` 套 `desc`。
fn create_pool(device: &Device, desc: &SetLayoutDesc, max_sets: u32) -> Result<vk::DescriptorPool> {
    let pool_sizes = desc.pool_sizes_vec(max_sets);
    let info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(max_sets)
        .pool_sizes(&pool_sizes);
    Ok(unsafe { device.create_descriptor_pool(&info, None) }?)
}

/// reset `pool` 后分配 `count` 套同布局 set。
/// 坑：重建时不先 reset 释放旧 set 就重分会 OUT_OF_POOL_MEMORY（resize 实测）。
fn realloc_sets(
    device: &Device,
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
    count: usize,
) -> Result<Vec<vk::DescriptorSet>> {
    let layouts = vec![layout; count];
    let info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    unsafe {
        device.reset_descriptor_pool(pool, vk::DescriptorPoolResetFlags::empty())?;
        Ok(device.allocate_descriptor_sets(&info)?)
    }
}

/// 录一条金字塔布局屏障，`range` 可跨多 mip（core 的 `ImageBarrier` 只管第 0 级）。
/// `layouts`/`stages`/`access` 都是 (前, 后)。
///
/// # Safety
/// `cb` 处于录制状态，`image` 合法。
unsafe fn pyramid_barrier(
    device: &Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    range: vk::ImageSubresourceRange,
    layouts: (vk::ImageLayout, vk::ImageLayout),
    stages: (vk::PipelineStageFlags, vk::PipelineStageFlags),
    access: (vk::AccessFlags, vk::AccessFlags),
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(layouts.0)
        .new_layout(layouts.1)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range)
        .src_access_mask(access.0)
        .dst_access_mask(access.1);
    unsafe {
        device.cmd_pipeline_barrier(
            cb,
            stages.0,
            stages.1,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            slice::from_ref(&barrier),
        );
    }
}

/// `image` 的 2D 视图。
///
/// # Safety
/// `image` 须为合法句柄且寿命覆盖返回视图的使用期。
unsafe fn create_view(
    device: &Device,
    image: vk::Image,
    format: vk::Format,
    range: vk::ImageSubresourceRange,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(range);
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

/// image 的 dedicated memory：DEVICE_LOCAL 选型 → 恰量分配 → offset 0 绑定（失败回收
/// memory，image 归调用方回收）。
///
/// # Safety
/// `image` 须为刚创建成功的合法句柄。
unsafe fn create_bound_memory(ctx: &Context, image: vk::Image) -> Result<vk::DeviceMemory> {
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
            return Err(VulkanError::Vk(e));
        }
        Ok(memory)
    }
}

#[cfg(test)]
mod tests {
    use super::{occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z};

    /// 一个 mip 目标像素覆盖的源像素范围（右端不含）。奇数源尺寸向下减半时，最后的
    /// 目标像素吸收末尾多出的一行/列。
    fn reduction_source_range(dst: u32, src_size: u32, dst_size: u32) -> std::ops::Range<u32> {
        let start = dst * 2;
        let end = if dst + 1 == dst_size {
            src_size
        } else {
            (start + 2).min(src_size)
        };
        start..end
    }

    fn reduce_mip_level(source: &[f32], src_dims: [u32; 2], dst_dims: [u32; 2]) -> Vec<f32> {
        let mut output = vec![1.0; (dst_dims[0] * dst_dims[1]) as usize];
        for y in 0..dst_dims[1] {
            for x in 0..dst_dims[0] {
                let xr = reduction_source_range(x, src_dims[0], dst_dims[0]);
                let yr = reduction_source_range(y, src_dims[1], dst_dims[1]);
                let mut minimum = 1.0_f32;
                for sy in yr {
                    for sx in xr.clone() {
                        minimum = minimum.min(source[(sy * src_dims[0] + sx) as usize]);
                    }
                }
                output[(y * dst_dims[0] + x) as usize] = minimum;
            }
        }
        output
    }

    /// reverse-Z 归约极值方向：tile 内 {0.2, 0.9, 0.5, 0.3} 的保守遮挡上界 = MIN = 0.2。
    /// 取错 = 系统性错杀，与 hiz_downsample.wgsl 的 min 同源。
    #[test]
    fn reduction_takes_min_for_reverse_z() {
        let tile = reduce_tile_reverse_z(0.2, 0.9, 0.5, 0.3);
        assert_eq!(tile, 0.2, "tile 保守遮挡上界 = 最远表面 = 值最小者");
        // 全背景 tile（clear 0 = 远平面）→ 上界 0
        assert_eq!(reduce_tile_reverse_z(0.0, 0.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn odd_mip_reduction_covers_every_source_texel() {
        for src_dims in [[225, 225], [400, 225], [25, 14], [3, 1], [1, 1], [2, 1]] {
            let dst_dims = [(src_dims[0] / 2).max(1), (src_dims[1] / 2).max(1)];
            let source: Vec<f32> = (0..src_dims[0] * src_dims[1])
                .map(|i| (i % 251) as f32 / 251.0)
                .collect();
            let actual = reduce_mip_level(&source, src_dims, dst_dims);
            let mut covered = vec![false; source.len()];
            for y in 0..dst_dims[1] {
                for x in 0..dst_dims[0] {
                    let xr = reduction_source_range(x, src_dims[0], dst_dims[0]);
                    let yr = reduction_source_range(y, src_dims[1], dst_dims[1]);
                    let mut expected = 1.0_f32;
                    for sy in yr {
                        for sx in xr.clone() {
                            let index = (sy * src_dims[0] + sx) as usize;
                            covered[index] = true;
                            expected = expected.min(source[index]);
                        }
                    }
                    assert_eq!(actual[(y * dst_dims[0] + x) as usize], expected);
                }
            }
            assert!(covered.into_iter().all(|covered| covered), "{src_dims:?}");
        }
    }

    /// 遮挡判定保守性：等号 = 可见（宁多画不错杀）。
    #[test]
    fn occlusion_verdict_conservative() {
        // 物体最近点（0.9）比 tile 最远缓冲表面（0.2）近得多 → 不可剔
        assert!(!occluded_reverse_z(0.9, 0.2));
        // 物体最近点（0.1）比 tile 最远缓冲表面（0.2）还远 → 全身被挡
        assert!(occluded_reverse_z(0.1, 0.2));
        // 等号 = 可见（保守：贴着挡板远沿的物体保留）
        assert!(!occluded_reverse_z(0.2, 0.2));
        // 背景 tile（min = 0，clear 远平面）→ 恒不剔除（0 < 0 不成立）
        assert!(!occluded_reverse_z(0.5, 0.0));
        // 越过近平面（深度值 > 1，clamp 免除——occluder ≤ 1 恒不剔）
        assert!(!occluded_reverse_z(1.3, 0.8));
    }

    /// mip 层选择：ceil(log2(最大边))，**下限 1**（level 0 不派发）、clamp 进级数。
    #[test]
    fn mip_level_selection() {
        assert_eq!(
            occlusion_mip_level(1.0, 1.0, 11),
            1,
            "≤2 px → level 1（下限）"
        );
        assert_eq!(occlusion_mip_level(128.0, 64.0, 11), 7, "128 px → 2^7");
        assert_eq!(occlusion_mip_level(64.0, 128.0, 11), 7, "最大边定层");
        assert_eq!(
            occlusion_mip_level(100.0, 100.0, 11),
            7,
            "ceil(log2 100) = 7"
        );
        assert_eq!(occlusion_mip_level(4096.0, 4096.0, 11), 10, "clamp 进级数");
        assert_eq!(occlusion_mip_level(0.0, 0.0, 11), 1, "退化 AABB → 下限 1");
    }
}
