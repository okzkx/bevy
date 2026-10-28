//! Hi-Z 深度金字塔（6.1.2）：上一帧深度归约成的层级深度图——遮挡测试的
//! 取数底座。R32_SFLOAT ×2 帧槽（与标记环/模型环同一条 fence 纪律），逐级
//! MIN 归约（reverse-Z 保守方向，本模块纯函数三件 + 单测钉死，判定线 3）。
//!
//! 时序形状（责任边界契约：每帧由 compute 链全量重写，非上传链产物）：
//! 帧内录制序 = [视锥剔除] → [遮挡测试（读**同槽**金字塔 = 两帧前本帧槽的
//! 归约输出，`wait_for_slot` 的 fence 已保证其写入提交完成——2 帧滞后零新
//! 同步形状，与标记环读数同纪律）] → [渲染写深度] → [归约链逐级重写本槽
//! 金字塔]。同帧内"遮挡读旧 → 归约写新"落在同一条命令缓冲，屏障收口。
//! 深度附件在本帧渲染后（storeOp STORE）转 DEPTH_READ_ONLY 供 level 0 归约
//! 采样，下帧 UNDEFINED 进场照常丢弃——深度内容不跨帧持久，金字塔才是
//! 持久层。
//!
//! 归约形状：逐级 dispatch（hiz_downsample.wgsl 单入口），不搬 bevy 的 SPD
//! 单 pass 批量重排——学机制与保守化，不搬整套架构（5.2 段计划同款取舍）。
//! 描述符：归约走独立小布局（b0 源采样 / b1 目标 storage，每 (帧槽,级) 一套
//! set）；遮挡消费走第二个独立小布局（b0 = 全链采样视图）。不进 set2——
//! 金字塔随 swapchain 重建会整体换 image/视图，set2"描述符 init 一次写入
//! 永不更新"的冻结纪律不为它开口，重建 churn 隔离在本模块内（resize 时
//! `rebuild` 重建 image/视图、reset pool 并重写描述符——管线与 pool 不随
//! extent 变，pool 按设备最大级数预留；温暖路径与深度附件同纪律：自带
//! device_wait_idle）。
//!
//! warmup（6.1.4 边界）：重建后两个帧槽的金字塔内容都是未定义（首建/
//! resize 后），前 2 帧遮挡测试整段跳过（fallback 判据之一，`ready()`）。

use ash::{vk, Device};
use bevy::log::info;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::common::error::{Result, VulkanError};

use crate::render::resident::images::full_mip_chain;
use crate::render::core::kit::compute_pipeline::ComputePipeline;
use crate::render::core::device::readback::OneShotReadback;
use crate::render::Context;

/// shader 模块字节（build.rs 产物；spirv-val 已过，naga textureLoad/
/// textureDimensions/textureNumLevels 的 mip 层参数发射已实证）。
const HIZ_DOWNSAMPLE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hiz_downsample.spv"));

/// 金字塔格式（R32_SFLOAT 单通道浮点——深度值域 [0,1] 直存，无语义压缩）。
/// 支持查询在 [`HizPyramid::new`]，STORAGE_IMAGE + SAMPLED_IMAGE + TRANSFER_SRC
/// 能力实测留痕（§2 核实项 2：能力不足即 Init 出局，不静默换格式）。
pub(crate) const HIZ_FORMAT: vk::Format = vk::Format::R32_SFLOAT;

// ===== 纯函数三件（判定线 3 的 CPU 参照与单测本体；occlusion.wgsl 注释同源）=====

/// 一个 mip 目标像素覆盖的源像素范围。奇数源尺寸向下减半时，最后的目标
/// 像素吸收末尾多出的一行/列，范围右端不包含。
#[cfg(test)]
fn reduction_source_range(dst: u32, src_size: u32, dst_size: u32) -> std::ops::Range<u32> {
    let start = dst * 2;
    let end = if dst + 1 == dst_size {
        src_size
    } else {
        (start + 2).min(src_size)
    };
    start..end
}

#[cfg(test)]
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

/// reverse-Z tile 归约：覆盖对应源区间并取 MIN。**方向钉死**：本仓深度
/// reverse-Z（近=1、远=0），值越小 = 越远；tile 的保守遮挡上界 = tile 内
/// 最远的已缓冲表面 = 值最小者。正向 Z 的同类归约取 MAX，方向相反。
#[must_use]
pub fn reduce_tile_reverse_z(a: f32, b: f32, c: f32, d: f32) -> f32 {
    a.min(b).min(c).min(d)
}

/// reverse-Z 遮挡判定：物体"最近角点"的深度值（深度值上界，见 occlusion.wgsl
/// 头注）小于覆盖区域的最远缓冲表面（tile MIN）→ 全身被挡。**等号 = 可见**
///（宁多画不错杀：恰好贴着挡板远沿的物体保留绘制）。
#[must_use]
pub fn occluded_reverse_z(obj_closest_ndc: f32, tile_min: f32) -> bool {
    obj_closest_ndc < tile_min
}

/// mip 层选择：`ceil(log2(AABB 像素最大边))`（bevy occlusion_culling.wesl
/// 同式）——该层 tile 边长 ≥ AABB/2，AABB 每轴至多横跨 2 tile，4 tap 的
/// 2×2 邻域必覆盖；**下限 1**（本仓金字塔 level 0 不派发无内容——取舍钉明
/// 见 [`HizPyramid::record_reduction`]），clamp 进级数。px ≤ 2 → level 1。
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

/// 一帧槽的金字塔 image + dedicated memory（DEVICE_LOCAL；生命周期随本结构，
/// 重建见 [`HizPyramid::rebuild`]）。
struct HizImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

/// 单帧槽的全部视图：遮挡测试读全链（一张 view 覆盖所有 mip 层，textureLoad
/// 按层取）；归约逐级两视图（源/目标）——源 level 0 = 深度附件采样视图
///（指向该帧槽自己的深度 image），level L≥1 = 上一级金字塔的采样视图。
struct HizViews {
    device: Device,
    /// 遮挡测试的全链采样视图（SHADER_READ，GENERAL 布局，全 mip 层）。
    occlusion: vk::ImageView,
    /// 归约目标 storage 视图（每级一张）。
    storage: Vec<vk::ImageView>,
    /// 归约源采样视图（每级一张；[0] = 深度附件视图，[L] = 金字塔 L-1 级）。
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

struct HizViewsBuildGuard {
    views: Vec<HizViews>,
}

impl Drop for HizViewsBuildGuard {
    fn drop(&mut self) {
        self.views.clear();
    }
}

/// 归约 compute 管线 + 描述符：单布局（b0 源采样 / b1 目标 storage），
/// 每 (帧槽, 级) 一套 set。管线/布局/pool 不依赖 extent，`new` 建一次跨重建
/// 复用；set 内容随 init/rebuild 重写，两次重写之间不变。句柄 null / None =
/// 未建成（Drop 照拆：destroy null 句柄是规范允许的 no-op）。
struct ReductionPass {
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    pipeline: Option<ComputePipeline>,
    /// pool 预留的级数上限（设备 maxImageDimension2D 的完整 mip 链）——
    /// 按首建级数定死会在窗口放大、级数变多时 OUT_OF_POOL_MEMORY。
    pool_levels: u32,
    /// set 下标 = slot × levels + level。
    sets: Vec<vk::DescriptorSet>,
}

/// Hi-Z 深度金字塔：×2 帧槽 image + 全部视图 + 归约管线 + 遮挡测试的
/// 全链采样描述符。作为 Resource 被 host 帧循环持有（`ResMut`——warmup/
/// initialized 旗标每帧走账）。
#[derive(bevy::prelude::Resource)]
pub struct HizPyramid {
    device: Device,
    slots: Vec<HizImage>,
    views: Vec<HizViews>,
    /// 每帧槽的金字塔已初始化旗标（首建/重建后 false → 首次归约插
    /// UNDEFINED→GENERAL 进场屏障；之后恒 GENERAL 只走内存屏障）。AtomicBool：
    /// record_reduction 以 &self 录制期置位（GpuTimer::recorded 同款纪律——
    /// 单线程录制，无竞争面，原子只为 Resource 的 Sync 约束）。
    initialized: Vec<AtomicBool>,
    /// 重建后的预热计数（>0 = 内容未就绪，遮挡测试 fallback；每次归约减一）。
    warmup: u32,
    levels: u32,
    extent: vk::Extent2D,
    reduction: ReductionPass,
    /// 遮挡测试的全链采样描述符（独立小布局：b0 = occlusion 视图，每帧槽一套）。
    occlusion_layout: vk::DescriptorSetLayout,
    occlusion_pool: vk::DescriptorPool,
    occlusion_sets: Vec<vk::DescriptorSet>,
}

/// 一次读回的统计行（判定线 3 的 CPU 参照通道读数口；只进计量/验证，
/// 不进绘制路径）。
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
    /// 读回指定级全量（原始 f32 行主序）——判定线 3 的 CPU 参照通道本体。
    /// 布局收口：GENERAL（恒定）→ TRANSFER_SRC_OPTIMAL（拷贝）→ 回 GENERAL；
    /// 一次性资源随调用生命周期（[`OneShotReadback`]，成败路径同一回收出口）。
    ///
    /// # Errors
    /// staging/命令池/fence 创建、录制、提交或映射失败。
    pub fn readback_level_raw(
        &self,
        ctx: &Context,
        slot: usize,
        level: u32,
    ) -> Result<(u32, u32, Vec<f32>)> {
        let (w, h) = self.level_dims(level);
        let slot = slot % self.slots.len();
        let byte_size = u64::from(w) * u64::from(h) * 4;
        // 录制：布局切 TRANSFER_SRC → 拷贝 → 切回 GENERAL（金字塔恒 GENERAL
        // 是运行时形状，读回借用后归还）。进场屏障 src 取 ALL_COMMANDS/
        // MEMORY_WRITE 宽口径：写者是前序提交里的归约 compute，提交序把它们
        // 纳入第一同步域
        let to_transfer = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.slots[slot].image)
            .subresource_range(single_level_range(level))
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        let to_general = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.slots[slot].image)
            .subresource_range(single_level_range(level))
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(level)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
        let mut bytes = vec![0u8; byte_size as usize];
        unsafe {
            let device = &ctx.device;
            let readback = OneShotReadback::begin(ctx, byte_size)?;
            device.cmd_pipeline_barrier(
                readback.cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
            device.cmd_copy_image_to_buffer(
                readback.cb,
                self.slots[slot].image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.staging,
                &[region],
            );
            device.cmd_pipeline_barrier(
                readback.cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_general],
            );
            readback.finish_and_read(ctx, &mut bytes)?;
        }
        let floats = bytes.as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect();
        Ok((w, h, floats))
    }

    /// 读回指定级并出统计（判定线 3 读数口；语义见 [`HizCensus`]）。
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

    /// 建金字塔：能力实测（R32_SFLOAT 的 STORAGE/SAMPLED/TRANSFER_SRC，§2
    /// 核实项 2）→ ×2 帧槽 image → 全部视图 → 归约管线与描述符 → 遮挡测试
    /// 描述符。`depth_images` = 各帧槽深度 image（level 0 归约源）。
    ///
    /// # Errors
    /// 格式能力不足（Init 出局——备选通道另立案，不静默换格式）或
    /// Vulkan 创建/分配/绑定失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D, depth_images: &[vk::Image]) -> Result<Self> {
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, HIZ_FORMAT)
        }
        .optimal_tiling_features;
        // 三用途逐一换算：compute 归约写（STORAGE_IMAGE）、遮挡/逐级读
        //（SAMPLED_IMAGE）、CPU 读回校验拷贝（TRANSFER_SRC，判定线 3）
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
            "Hi-Z 格式能力实测: {HIZ_FORMAT:?} optimal features 含 STORAGE_IMAGE | SAMPLED_IMAGE | TRANSFER_SRC（{levels} 级，§2 核实项 2 留痕）"
        );
        let mut this = Self {
            device: ctx.device.clone(),
            slots: Vec::new(),
            views: Vec::new(),
            initialized: (0..crate::render::MAX_FRAMES_IN_FLIGHT).map(|_| AtomicBool::new(false)).collect(),
            warmup: crate::render::MAX_FRAMES_IN_FLIGHT as u32,
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
        this.build_occlusion_sets(ctx, depth_images)?;
        info!(
            "Hi-Z 金字塔就绪: {HIZ_FORMAT:?} {}x{} ×{levels} 级 ×{MAX} 帧槽（每帧渲染后逐级归约重写；\
             遮挡测试读同槽两帧前输出——fence 纪律零新同步形状；重建 warmup {MAX} 帧）",
            extent.width,
            extent.height,
            MAX = crate::render::MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 金字塔 mip 级数。
    #[must_use]
    pub fn levels(&self) -> u32 {
        self.levels
    }

    /// 金字塔内容是否就绪（重建 warmup 归零）——遮挡测试的 fallback 判据之一。
    #[must_use]
    pub fn ready(&self) -> bool {
        self.warmup == 0
    }

    /// 第 `slot` 帧槽的遮挡测试描述符集（b0 = 全链采样视图）。
    #[must_use]
    pub fn occlusion_set(&self, slot: usize) -> vk::DescriptorSet {
        self.occlusion_sets[slot]
    }

    /// 遮挡测试的描述符集布局（建遮挡管线 layout 的素材）。
    #[must_use]
    pub fn occlusion_layout(&self) -> vk::DescriptorSetLayout {
        self.occlusion_layout
    }

    /// 一次归约链录制完成后的走账：warmup 递减（ready 判据）。
    pub fn note_reduced(&mut self) {
        self.warmup = self.warmup.saturating_sub(1);
    }

    /// resize 随 swapchain 重建（幂等：extent 没变就空手而归）。与深度附件
    /// 同纪律：销毁旧资源前自带 device_wait_idle（在飞命令可能还引用旧
    /// image/视图）；重建后 warmup 重置（遮挡 fallback 到首次归约 + 2 帧）。
    ///
    /// # Errors
    /// 等待排空或新资源创建失败。
    pub fn rebuild(&mut self, ctx: &Context, extent: vk::Extent2D, depth_images: &[vk::Image]) -> Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        self.drop_resources();
        self.extent = extent;
        self.levels = full_mip_chain(extent.width, extent.height);
        self.initialized = (0..self.slots_capacity()).map(|_| AtomicBool::new(false)).collect();
        self.warmup = crate::render::MAX_FRAMES_IN_FLIGHT as u32;
        self.build_images_and_views(ctx, depth_images)?;
        self.write_reduction_sets(ctx)?;
        self.build_occlusion_sets(ctx, depth_images)?;
        info!(
            "Hi-Z 金字塔随 swapchain 重建: {}x{} ×{} 级（{} 帧槽，warmup 重置，遮挡测试随后就绪）",
            extent.width,
            extent.height,
            self.levels,
            self.slots_capacity(),
        );
        Ok(())
    }

    fn slots_capacity(&self) -> usize {
        crate::render::MAX_FRAMES_IN_FLIGHT
    }

    /// 录制一帧的归约链（渲染进场收口后调用）：每级绑 (slot, level) set →
    /// dispatch；级间内存屏障（本级写 → 下级读）。调用方保证已做深度屏障
    ///（depth 附件 → DEPTH_READ_ONLY）与本槽进场屏障（本函数开头插）。
    pub(crate) fn record_reduction(&self, device: &Device, cb: vk::CommandBuffer, slot: usize) {
        // new 成功即 Some；None 只出现在 new 半建失败后的 Drop 路径，录制不可达
        let Some(reduction) = self.reduction.pipeline.as_ref().map(ComputePipeline::handles) else {
            return;
        };
        let slot = slot % self.slots.len();
        unsafe {
            // 进场：首用 UNDEFINED→GENERAL；之后恒 GENERAL，只收口
            // "遮挡测试读（本帧槽上上轮的读者，fence 保证已完成——同槽读者
            // 在本帧命令缓冲更早处）→ 归约写"的内存序。
            if !self.initialized[slot].swap(true, Ordering::Relaxed) {
                let barrier = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.slots[slot].image)
                    .subresource_range(full_range(self.levels))
                    .dst_access_mask(vk::AccessFlags::SHADER_WRITE);
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    std::slice::from_ref(&barrier),
                );
            } else {
                let barrier = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.slots[slot].image)
                    .subresource_range(full_range(self.levels))
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::SHADER_WRITE);
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    std::slice::from_ref(&barrier),
                );
            }
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, reduction.pipeline);
            // **级序取舍钉明（6.1.2 施工订正）**：从 level 1 起派发（level 1 =
            // 深度 2×2 MIN），**level 0 不派发**——原形状把 2×2 MIN 写进全尺寸
            // level 0，只有左上 1/4 被写、其余未定义，整条链读垃圾（运行实抓：
            // 高层 tap 读 0、遮挡恒可见）。bevy 的 mip0=深度本体形状不搬——
            // 那要一次全尺寸拷贝 dispatch；本仓以"level 0 无内容、可查层从
            // 1 起（2px tile）"为代价省掉它，occlusion 侧 mip 选择下限 1。
            for level in 1..self.levels.max(1) {
                let set = self.reduction.sets[slot * self.levels as usize + level as usize];
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::COMPUTE,
                    reduction.layout,
                    0,
                    std::slice::from_ref(&set),
                    &[],
                );
                let (w, h) = self.level_dims(level);
                device.cmd_dispatch(cb, w.div_ceil(8), h.div_ceil(8), 1);
                // 级间屏障：本级写 → 下级读（下一 dispatch 的 textureLoad）
                if level + 1 < self.levels {
                    let barrier = vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ);
                    device.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::DependencyFlags::empty(),
                        std::slice::from_ref(&barrier),
                        &[],
                        &[],
                    );
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

    /// image + 视图 + 内存：×2 帧槽，重建路径复用（drop_resources 已先清）。
    fn build_images_and_views(&mut self, ctx: &Context, depth_images: &[vk::Image]) -> Result<()> {
        let device = &ctx.device;
        let mut slots = Vec::new();
        for _ in 0..crate::render::MAX_FRAMES_IN_FLIGHT {
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
            // 失败路径回收 image（memory/view 由局部建链自拆）
            match unsafe { create_bound_memory(ctx, image) } {
                Ok(memory) => slots.push(HizImage {
                    device: device.clone(),
                    image,
                    memory,
                }),
                Err(e) => {
                    unsafe { device.destroy_image(image, None) };
                    for s in slots {
                        drop(s);
                    }
                    return Err(e);
                }
            }
        }
        self.slots = slots;
        let mut guard = HizViewsBuildGuard {
            views: Vec::with_capacity(self.slots.len()),
        };
        for (slot, hiz_image) in self.slots.iter().enumerate() {
            let occlusion = unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(hiz_image.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(HIZ_FORMAT)
                        .subresource_range(full_range(self.levels)),
                    None,
                )
            }
            .map_err(VulkanError::from)?;
            let mut current = HizViews {
                device: device.clone(),
                occlusion,
                storage: Vec::with_capacity(self.levels as usize),
                src: Vec::with_capacity(self.levels as usize),
            };
            for level in 0..self.levels {
                current.storage.push(unsafe { create_level_view(device, hiz_image.image, level) }?);
                // 源视图：dispatch 从 level 1 起——level 0/1 的源 = 深度附件
                //（D32，DEPTH aspect；[0] 槽位不派发，占位同深度），L≥2 = 上一级
                let source = if level <= 1 {
                    unsafe { create_depth_sample_view(device, depth_images[slot]) }
                } else {
                    unsafe { create_level_view(device, hiz_image.image, level - 1) }
                };
                current.src.push(source?);
            }
            guard.views.push(current);
        }
        self.views = std::mem::take(&mut guard.views);
        Ok(())
    }

    /// 归约管线 + set layout + 描述符 pool（`new` 建一次，跨重建复用——都不
    /// 依赖 extent）。每建成一个句柄即落进 `self.reduction`，中途失败由 Drop
    /// 回收已建部分。pool 按设备 maxImageDimension2D 的完整 mip 链预留，任意
    /// 合法 extent 的级数都装得下，rebuild 只 reset 不重建。
    fn build_reduction_pipeline(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        self.reduction.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        self.reduction.pipeline = Some(ComputePipeline::new(
            device,
            HIZ_DOWNSAMPLE_SPV,
            std::slice::from_ref(&self.reduction.set_layout),
            0,
            c"cs_main",
        )?);
        let max_dim = ctx.limits().max_image_dimension2_d;
        self.reduction.pool_levels = full_mip_chain(max_dim, max_dim);
        let max_sets = crate::render::MAX_FRAMES_IN_FLIGHT as u32 * self.reduction.pool_levels;
        self.reduction.pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(max_sets)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLED_IMAGE)
                            .descriptor_count(max_sets),
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::STORAGE_IMAGE)
                            .descriptor_count(max_sets),
                    ]),
                None,
            )
        }?;
        Ok(())
    }

    /// 归约描述符：reset pool → 每 (帧槽,级) 分配一套 set → 写入当前视图
    ///（首建与 rebuild 共用；调用前视图已就位）。
    fn write_reduction_sets(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        if self.levels > self.reduction.pool_levels {
            return Err(VulkanError::Init(format!(
                "Hi-Z {} 级超出描述符 pool 预留的 {} 级（extent 超过设备 maxImageDimension2D）",
                self.levels, self.reduction.pool_levels
            )));
        }
        let sets_total = crate::render::MAX_FRAMES_IN_FLIGHT * self.levels as usize;
        self.reduction.sets.clear();
        unsafe { device.reset_descriptor_pool(self.reduction.pool, vk::DescriptorPoolResetFlags::empty()) }
            .map_err(VulkanError::from)?;
        let sets_layouts = std::iter::repeat_n(self.reduction.set_layout, sets_total).collect::<Vec<_>>();
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.reduction.pool)
                    .set_layouts(&sets_layouts),
            )
        }
        .map_err(VulkanError::from)?;
        // 写入：set(slot,level) b0 = 源视图、b1 = 目标 storage 视图。布局声明
        // 逐源对应（VUID-00344：描述符声明的 layout 须与 image 实际布局一致）
        // ——level 0/1 源 = 深度附件（DEPTH_READ_ONLY，frame_draw.rs 帧尾屏障落位；
        // [0] 槽位不派发），其余 = 金字塔（恒 GENERAL）
        let mut image_infos = Vec::with_capacity(sets_total * 2);
        let mut meta = Vec::with_capacity(sets_total * 2);
        for slot in 0..crate::render::MAX_FRAMES_IN_FLIGHT {
            for level in 0..self.levels as usize {
                let set = sets[slot * self.levels as usize + level];
                image_infos.push(
                    vk::DescriptorImageInfo::default()
                        .image_view(self.views[slot].src[level])
                        .image_layout(if level <= 1 {
                            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL
                        } else {
                            vk::ImageLayout::GENERAL
                        }),
                );
                meta.push((set, 0u32, false));
                image_infos.push(
                    vk::DescriptorImageInfo::default()
                        .image_view(self.views[slot].storage[level])
                        .image_layout(vk::ImageLayout::GENERAL),
                );
                meta.push((set, 1u32, true));
            }
        }
        let writes: Vec<vk::WriteDescriptorSet> = meta
            .iter()
            .zip(&image_infos)
            .map(|((set, binding, storage), info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(*binding)
                    .descriptor_type(if *storage {
                        vk::DescriptorType::STORAGE_IMAGE
                    } else {
                        vk::DescriptorType::SAMPLED_IMAGE
                    })
                    .image_info(std::slice::from_ref(info))
            })
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        self.reduction.sets = sets;
        Ok(())
    }

    /// 遮挡测试的全链采样描述符（b0 = occlusion 视图，每帧槽一套）。
    fn build_occlusion_sets(&mut self, ctx: &Context, _depth_images: &[vk::Image]) -> Result<()> {
        let device = &ctx.device;
        // pool 首建/重建分流：重建路径先 reset（释放旧 2 套再分——否则
        // OUT_OF_POOL_MEMORY，resize 负例实抓）
        if self.occlusion_pool == vk::DescriptorPool::null() {
            let binding = vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE);
            self.occlusion_layout = unsafe {
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[binding]),
                    None,
                )
            }
            .map_err(VulkanError::from)?;
            self.occlusion_pool = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(crate::render::MAX_FRAMES_IN_FLIGHT as u32)
                        .pool_sizes(&[vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLED_IMAGE)
                            .descriptor_count(crate::render::MAX_FRAMES_IN_FLIGHT as u32)]),
                    None,
                )
            }
            .map_err(VulkanError::from)?;
        } else {
            unsafe {
                device.reset_descriptor_pool(self.occlusion_pool, vk::DescriptorPoolResetFlags::empty())
            }
            .map_err(VulkanError::from)?;
        }
        let occlusion_layouts =
            std::iter::repeat_n(self.occlusion_layout, crate::render::MAX_FRAMES_IN_FLIGHT).collect::<Vec<_>>();
        self.occlusion_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.occlusion_pool)
                    .set_layouts(&occlusion_layouts),
            )
        }
        .map_err(VulkanError::from)?;
        let image_infos: Vec<vk::DescriptorImageInfo> = self
            .views
            .iter()
            .map(|v| {
                vk::DescriptorImageInfo::default()
                    .image_view(v.occlusion)
                    .image_layout(vk::ImageLayout::GENERAL)
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = self
            .occlusion_sets
            .iter()
            .zip(&image_infos)
            .map(|(set, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(std::slice::from_ref(info))
            })
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    fn drop_resources(&mut self) {
        self.views.clear();
        for slot in self.slots.drain(..) {
            drop(slot);
        }
        // reduction pipeline/layout/pool 与 occlusion pool/layout 跨重建复用
        //（pool 只 reset、set 重写，随 Self Drop 终拆）——image/视图才是每重建
        // churn 的部分
    }
}

impl Drop for HizImage {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——重建路径的等待在 rebuild（device_wait_idle），
        // 退出排空在 teardown_vulkan（先于本结构 Drop）。
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

impl Drop for HizPyramid {
    fn drop(&mut self) {
        self.drop_resources();
        let device = &self.device;
        // 句柄逐个落位（null = 未建成——new 中途失败时 Drop 照常走这里）；
        // destroy null 句柄是规范允许的 no-op，null 检查只为读起来显式
        self.reduction.pipeline = None;
        if self.reduction.set_layout != vk::DescriptorSetLayout::null() {
            unsafe { device.destroy_descriptor_set_layout(self.reduction.set_layout, None) };
        }
        if self.reduction.pool != vk::DescriptorPool::null() {
            unsafe { device.destroy_descriptor_pool(self.reduction.pool, None) };
        }
        if self.occlusion_pool != vk::DescriptorPool::null() {
            unsafe { device.destroy_descriptor_pool(self.occlusion_pool, None) };
        }
        if self.occlusion_layout != vk::DescriptorSetLayout::null() {
            unsafe { device.destroy_descriptor_set_layout(self.occlusion_layout, None) };
        }
    }
}

/// 全 mip 层的 subresource range（COLOR aspect——R32_SFLOAT 是颜色格式）。
fn full_range(levels: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(levels)
        .layer_count(1)
}

/// 单 mip 层的 subresource range（读回拷贝用）。
fn single_level_range(level: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(level)
        .level_count(1)
        .layer_count(1)
}

/// 单级 mip 的采样视图（金字塔级间链）。
///
/// # Safety
/// `image` 须为合法句柄且寿命覆盖返回视图的使用期。
unsafe fn create_level_view(device: &Device, image: vk::Image, level: u32) -> Result<vk::ImageView> {
    unsafe {
        device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(HIZ_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_mip_level(level)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
            .map_err(VulkanError::from)
    }
}

/// 深度附件的采样视图（D32，DEPTH aspect，level 0）——归约 level 0 的源。
///
/// # Safety
/// `image` 须为合法深度 image 句柄且寿命覆盖视图使用期。
unsafe fn create_depth_sample_view(device: &Device, image: vk::Image) -> Result<vk::ImageView> {
    unsafe {
        device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(crate::render::DEPTH_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
            .map_err(VulkanError::from)
    }
}

/// image 的 dedicated memory：requirements 查询 → DEVICE_LOCAL 选型 → 恰量
/// 分配 → offset 0 绑定（DepthTarget 同款；失败回收 memory）。
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
    use super::{
        occluded_reverse_z, occlusion_mip_level, reduce_mip_level, reduce_tile_reverse_z,
        reduction_source_range,
    };

    /// **reverse-Z 归约极值方向（判定线 3，最危险单点）**：tile 内 2×2 深度
    /// 值 {0.2, 0.9, 0.5, 0.3} 的保守遮挡上界 = MIN = 0.2（值小 = 远——
    /// 0.2 是 tile 里"最远"的缓冲表面）。正向 Z 的同类归约取 MAX，方向
    /// 相反——本测试钉死本仓取 MIN，与 hiz_downsample.wgsl 的 min 同源。
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

    /// **遮挡判定保守性（判定线 3）**：物体最近角点深度值 vs tile 上界的
    /// 四态。等号 = 可见（宁多画不错杀）。
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

    /// mip 层选择：bevy 同式 ceil(log2(最大边))，**下限 1**（level 0 不派发）、
    /// clamp 进级数。
    #[test]
    fn mip_level_selection() {
        assert_eq!(occlusion_mip_level(1.0, 1.0, 11), 1, "≤2 px → level 1（下限）");
        assert_eq!(occlusion_mip_level(128.0, 64.0, 11), 7, "128 px → 2^7");
        assert_eq!(occlusion_mip_level(64.0, 128.0, 11), 7, "最大边定层");
        assert_eq!(occlusion_mip_level(100.0, 100.0, 11), 7, "ceil(log2 100) = 7");
        assert_eq!(occlusion_mip_level(4096.0, 4096.0, 11), 10, "clamp 进级数");
        assert_eq!(occlusion_mip_level(0.0, 0.0, 11), 1, "退化 AABB → 下限 1");
    }
}
