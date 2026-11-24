//! RT 描述符集与调试图（9.4.6）：独立 compute 的求交调试视图。调试图不接任何绘制 pass
//! ——含 `RayQueryKHR` 的 shader 在非 RT 设备上建不了管线，塞进后链会破坏"RT 关 = 9.3
//! 逐位一致"的降级路径。
//!
//! 本模块持有：
//! - [`RtTargets`]：按帧槽各一张 RGBA16F 调试图 + 统计缓冲（原子计数）+ RT 描述符集；
//!   随 swapchain 尺寸重建，TLAS/网格池句柄变化时重写描述符（`ensure_bindings`）。
//! - [`RtPipelines`]：`tlas_build`（实例数组组装）与 `ray_debug`（主射线）两条 compute 管线。
//! - [`RtFrameDraw`]：② 段录制束（BLAS 待建命令 + TLAS 构建 + 调试图 dispatch）。
//!
//! RT 描述符集布局：b0 TLAS / b1 顶点池 storage / b2 索引池 storage / b3 调试图 /
//! b4 统计（u32×2：射线数、命中数）。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::put_u32s;
use crate::common::error::Result;

use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts,
};
use crate::render::core::binding::shader_abi;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::core::target::color::ColorTarget;
use crate::render::resident::assets::pool::MeshPool;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::rt::{BlasRegistry, TlasFrame};
use crate::render::{Context, HDR_FORMAT, MAX_FRAMES_IN_FLIGHT};

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// RT 描述符集：b0 TLAS / b1 顶点池 / b2 索引池 / b3 调试图 / b4 统计。
pub(crate) static RT_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "rt",
    &[
        BindingDesc::new(0, DescKind::AccelerationStructure, COMPUTE),
        BindingDesc::new(1, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(2, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(3, DescKind::StorageImage, COMPUTE),
        BindingDesc::new(4, DescKind::StorageBuffer, COMPUTE),
    ],
);

/// TLAS 实例数组组装（set2 单集 + 4B push count）。
pub(crate) static TLAS_BUILD_PIPELINE: PipelineDesc = PipelineDesc {
    label: "tlas_build",
    shader: &shader_abi::TLAS_BUILD,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC],
    push: Some(PushRange {
        stages: COMPUTE,
        size: TLAS_BUILD_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 调试图（set0 实例表 / set1 帧 UBO / set2 RT 集 + 16B push）。
pub(crate) static RAY_DEBUG_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ray_debug",
    shader: &shader_abi::RAY_DEBUG,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC, &FRAME_UNIFORM_SET_DESC, &RT_SET_DESC],
    push: Some(PushRange {
        stages: COMPUTE,
        size: RAY_DEBUG_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// TLAS 组装的 push 字节：count（u32）。
pub const TLAS_BUILD_PUSH_SIZE: u32 = 4;
/// 调试图的 push 字节：width / height / view / pad。
pub const RAY_DEBUG_PUSH_SIZE: u32 = 16;

/// 统计缓冲字节数（两个 u32：射线数、命中数）。
pub const RT_STATS_SIZE: u64 = 8;

/// 两条 RT compute 管线的持有者（进程级）。
#[derive(bevy::prelude::Resource)]
pub struct RtPipelines {
    tlas_build: ComputePipeline,
    ray_debug: ComputePipeline,
}

impl RtPipelines {
    /// # Errors
    /// 配方对账不过或管线创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let tlas_build = ComputePipeline::new(ctx, &TLAS_BUILD_PIPELINE, layouts)?;
        let ray_debug = ComputePipeline::new(ctx, &RAY_DEBUG_PIPELINE, layouts)?;
        info!("RT 管线就绪: tlas_build（set2 + 4B push）+ ray_debug（set0/set1/set2 + 16B push）");
        Ok(Self {
            tlas_build,
            ray_debug,
        })
    }

    #[must_use]
    pub fn tlas_build(&self) -> ComputeHandles {
        self.tlas_build.handles()
    }

    #[must_use]
    pub fn ray_debug(&self) -> ComputeHandles {
        self.ray_debug.handles()
    }
}

/// 调试图 + 统计缓冲 + RT 描述符集（resize 级；尺寸随 swapchain 重建）。
#[derive(bevy::prelude::Resource)]
pub struct RtTargets {
    device: Device,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    images: Vec<ColorTarget>,
    stats: Vec<GpuBuffer>,
    extent: vk::Extent2D,
    /// 上次写进描述符的 (顶点池, 索引池, TLAS[0], TLAS[1])：句柄变了才重写。
    bound: Option<(
        vk::Buffer,
        vk::Buffer,
        vk::AccelerationStructureKHR,
        vk::AccelerationStructureKHR,
    )>,
    /// 上一帧的实例数（TLAS 构建条数；BRP rt_stats 读数）。
    last_instances: u32,
}

impl RtTargets {
    /// 建空壳（描述符集按布局分配，但先不写）：写集要等 TLAS 与网格池句柄齐备。
    ///
    /// # Errors
    /// 布局/池/集创建失败或调试图创建失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D) -> Result<Self> {
        let layout = RT_SET_DESC.create(&ctx.device)?;
        let pool_sizes = RT_SET_DESC.pool_sizes_vec(MAX_FRAMES_IN_FLIGHT as u32);
        let pool = unsafe {
            ctx.device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .inspect_err(|_| ctx.device.destroy_descriptor_set_layout(layout, None))?
        };
        let sets = match unsafe {
            ctx.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&[layout; MAX_FRAMES_IN_FLIGHT]),
            )
        } {
            Ok(sets) => sets,
            Err(e) => {
                unsafe {
                    ctx.device.destroy_descriptor_pool(pool, None);
                    ctx.device.destroy_descriptor_set_layout(layout, None);
                }
                return Err(e.into());
            }
        };
        let mut this = Self {
            device: ctx.device.clone(),
            layout,
            pool,
            sets,
            images: Vec::new(),
            stats: Vec::new(),
            extent,
            bound: None,
            last_instances: 0,
        };
        this.rebuild_targets(ctx, extent)?;
        Ok(this)
    }

    /// 调试图与统计缓冲重建（resize 链调用；幂等——尺寸没变直接返回）。销毁旧资源前
    /// 自带 device_wait_idle（在飞帧不会再引用旧图/旧集）。
    ///
    /// # Errors
    /// 等待排空或调试图创建失败。
    pub fn rebuild(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if extent == self.extent && !self.images.is_empty() {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        self.images.clear();
        self.stats.clear();
        self.bound = None;
        self.rebuild_targets(ctx, extent)
    }

    fn rebuild_targets(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            self.images.push(ColorTarget::new_storage(
                ctx,
                extent,
                HDR_FORMAT,
                "RT 调试图",
            )?);
        }
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            self.stats.push(GpuBuffer::create(
                &self.device,
                ctx.memory_contract(),
                RT_STATS_SIZE,
                BufferRole::Storage,
            )?);
        }
        self.extent = extent;
        info!(
            "RT 调试图就绪: {}×{} RGBA16F storage ×{MAX_FRAMES_IN_FLIGHT} 帧槽 + 统计缓冲 {RT_STATS_SIZE}B/槽",
            extent.width, extent.height
        );
        Ok(())
    }

    /// 句柄齐备时写 RT 描述符集（全部 binding 一次写完）；任一句柄变化才重写。
    /// 返回是否已就绪（false = 网格池还没建，本帧跳过 RT 录制）。
    ///
    /// # Errors
    /// 描述符写入校验失败（声明/覆盖不完备）。
    pub fn ensure_bindings(
        &mut self,
        tlas: [vk::AccelerationStructureKHR; MAX_FRAMES_IN_FLIGHT],
        pool: &MeshPool,
    ) -> Result<bool> {
        let (Some(vertex), Some(index)) = (pool.vertex_buffer_info(), pool.index_buffer_info())
        else {
            return Ok(false);
        };
        let key = (vertex.0, index.0, tlas[0], tlas[1]);
        if self.bound == Some(key) {
            return Ok(true);
        }
        for (frame, set) in self.sets.iter().enumerate() {
            let mut writer = DescriptorWriter::new(&RT_SET_DESC, *set);
            writer
                .acceleration_structure(0, tlas[frame])
                .buffer(1, vertex.0, 0, vertex.1)
                .buffer(2, index.0, 0, index.1)
                .image(3, self.images[frame].view(), vk::ImageLayout::GENERAL)
                .buffer(4, self.stats[frame].buffer(), 0, RT_STATS_SIZE);
            writer.submit(&self.device)?;
        }
        self.bound = Some(key);
        info!(
            "RT 描述符集写入: TLAS×{MAX_FRAMES_IN_FLIGHT} + 顶点池 {}B + 索引池 {}B（网格池/TLAS 句柄变化后重写）",
            vertex.1, index.1
        );
        Ok(true)
    }

    /// 第 `frame` 帧槽的 RT 集。
    #[must_use]
    pub fn set(&self, frame: usize) -> DescSet {
        DescSet::new(&RT_SET_DESC, self.sets[frame])
    }

    /// set 布局（登记给管线组装）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&RT_SET_DESC, self.layout)
    }

    /// 第 `frame` 帧槽的调试图（`rt_dump` 读回）。
    #[must_use]
    pub(crate) fn debug_image(&self, frame: usize) -> &ColorTarget {
        &self.images[frame]
    }

    /// 第 `frame` 帧槽的统计缓冲（宿主清零 + BRP 读数）。
    pub fn stats_buffer(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.stats[frame]
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 记下本帧 TLAS 实例数（BRP 读数）。
    pub fn note_instances(&mut self, instances: u32) {
        self.last_instances = instances;
    }

    /// 上一帧的 TLAS 实例数。
    #[must_use]
    pub fn last_instances(&self) -> u32 {
        self.last_instances
    }

    /// 宿主清零本帧统计（wait_for_slot 之后调，GPU 上一轮已用完本槽）。
    ///
    /// # Errors
    /// 写失败。
    pub fn clear_stats(&mut self, frame: usize) -> Result<()> {
        self.stats[frame].write(0, &[0u8; RT_STATS_SIZE as usize])
    }
}

impl Drop for RtTargets {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.layout, None);
        }
    }
}

/// TLAS 构建 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct TlasBuildDraw {
    pub pass: ComputeHandles,
    /// 4B push：在场行数。
    pub push: [u8; TLAS_BUILD_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(在场行数 / 64)。
    pub groups: u32,
    /// 实例数组的设备地址（`InstanceTables` 现取）。
    pub instance_buffer_address: vk::DeviceAddress,
    pub instance_count: u32,
}

/// 调试图 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct RayDebugDraw {
    pub pass: ComputeHandles,
    /// 16B push：width / height / view / pad。
    pub push: [u8; RAY_DEBUG_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(像素数 / 64)。
    pub groups: u32,
    /// 本帧槽的 RT 描述符集。
    pub rt_set: DescSet,
    /// 本帧槽的调试图（写入前录 UNDEFINED→GENERAL 迁移）。
    pub image: vk::Image,
}

/// ② 段 RT 录制束：BLAS 待建命令 + TLAS 组装/构建 + 调试图 dispatch。
pub struct RtFrameDraw<'a> {
    /// BLAS 注册表（录待建命令要 &mut：待建条目转正）。
    pub blas: &'a mut BlasRegistry,
    /// 本帧 TLAS（录构建命令）。
    pub tlas: &'a TlasFrame,
    /// 本帧网格池设备基址（顶点, 索引）：BLAS 构建输入地址 = 基址 + 槽内偏移。
    /// 池扩容迁移后地址会变，每帧现取（排建时存绝对地址会指向已销毁的 buffer）。
    pub blas_addresses: (vk::DeviceAddress, vk::DeviceAddress),
    /// TLAS 实例数组组装（在场行为 0 时 None：空场景不录）。
    pub tlas_build: Option<TlasBuildDraw>,
    /// 调试图（RT 关/未就绪时 None）。
    pub debug: Option<RayDebugDraw>,
}

impl<'a> RtFrameDraw<'a> {
    /// 从 [`RayScene`] 拆出录制所需的两个借：BLAS 表可变、TLAS 只读。
    #[must_use]
    pub fn new(
        scene: &'a mut crate::render::rt::RayScene,
        blas_addresses: (vk::DeviceAddress, vk::DeviceAddress),
        tlas_build: Option<TlasBuildDraw>,
        debug: Option<RayDebugDraw>,
    ) -> Self {
        let (blas, tlas) = scene.split();
        Self {
            blas,
            tlas,
            blas_addresses,
            tlas_build,
            debug,
        }
    }
}

impl RtFrameDraw<'_> {
    /// 录 TLAS 段：BLAS 待建命令 → 屏障 → TLAS 组装 compute → 屏障 → TLAS 构建 → 屏障。
    /// 所有命令都在本帧命令缓冲内，受帧 fence 约束。
    pub(crate) fn record_tlas(
        &mut self,
        device: &Device,
        cb: vk::CommandBuffer,
        instance_set: DescSet,
        slot: usize,
    ) {
        self.blas
            .record_builds(cb, self.blas_addresses.0, self.blas_addresses.1);
        if let Some(build) = &self.tlas_build {
            build.pass.record(
                device,
                cb,
                &[instance_set],
                &build.push,
                build.groups,
                Handoff::ComputeReadWrite,
            );
            self.tlas.record_build(
                cb,
                slot,
                build.instance_buffer_address,
                build.instance_count,
            );
        }
    }

    /// 录调试图段：首帧布局迁移（UNDEFINED→GENERAL）→ 主射线 dispatch。
    pub(crate) fn record_debug(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        instance_set: DescSet,
        frame_set: DescSet,
    ) {
        let Some(debug) = &self.debug else {
            return;
        };
        // 整张图每帧重写，旧内容不要：UNDEFINED 进场只等前面的命令走完
        unsafe {
            ImageBarrier::transition(
                debug.image,
                vk::ImageAspectFlags::COLOR,
                (vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL),
                (
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                ),
                (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_WRITE),
            )
            .record(device, cb);
        }
        debug.pass.record(
            device,
            cb,
            &[instance_set, frame_set, debug.rt_set],
            &debug.push,
            debug.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 组一帧 TLAS 组装的 push 与组数（在场行数 0 = None）。
#[must_use]
pub fn tlas_build_draw(
    pass: ComputeHandles,
    instance_count: u32,
    instance_buffer_address: vk::DeviceAddress,
) -> Option<TlasBuildDraw> {
    if instance_count == 0 {
        return None;
    }
    let mut push = [0u8; TLAS_BUILD_PUSH_SIZE as usize];
    push[..4].copy_from_slice(&instance_count.to_le_bytes());
    Some(TlasBuildDraw {
        pass,
        push,
        groups: instance_count.div_ceil(64),
        instance_buffer_address,
        instance_count,
    })
}

/// 组调试图的 push（width/height/view）与组数。
#[must_use]
pub fn ray_debug_draw(
    pass: ComputeHandles,
    rt_set: DescSet,
    image: vk::Image,
    extent: vk::Extent2D,
    view: u32,
) -> RayDebugDraw {
    let mut push = [0u8; RAY_DEBUG_PUSH_SIZE as usize];
    put_u32s(&mut push, 0, &[extent.width, extent.height, view, 0]);
    let pixels = u64::from(extent.width) * u64::from(extent.height);
    RayDebugDraw {
        pass,
        push,
        groups: pixels.div_ceil(64) as u32,
        rt_set,
        image,
    }
}

/// 调试图 → RGBA16F 原始字节（BRP `rt_dump` 用）：一次性命令缓冲拷贝回宿主。
/// 帧内容 = 该帧槽上一次提交的调试图（两帧内新鲜）。
///
/// # Errors
/// 一次性回读资源创建/提交/等待失败。
pub fn readback_debug_image(ctx: &Context, targets: &RtTargets, slot: usize) -> Result<Vec<u8>> {
    use crate::render::core::device::readback::OneShotReadback;
    use crate::render::core::record::barrier::MemoryBarrierDesc;

    let extent = targets.extent();
    let byte_size = u64::from(extent.width) * u64::from(extent.height) * 8; // RGBA16F = 8B/px
    let mut out = vec![0u8; byte_size as usize];
    let device = &ctx.device;
    let image = targets.debug_image(slot).image();
    unsafe {
        let readback = OneShotReadback::begin(ctx, byte_size)?;
        // 前序 compute 写 → 拷贝读（跨提交的全局内存依赖）
        MemoryBarrierDesc {
            src_stage: vk::PipelineStageFlags::ALL_COMMANDS,
            src_access: vk::AccessFlags::MEMORY_WRITE,
            dst_stage: vk::PipelineStageFlags::TRANSFER,
            dst_access: vk::AccessFlags::TRANSFER_READ,
        }
        .record(device, readback.cb);
        ImageBarrier::transition(
            image,
            vk::ImageAspectFlags::COLOR,
            (
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            ),
            (
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::TRANSFER,
            ),
            (
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::TRANSFER_READ,
            ),
        )
        .record(device, readback.cb);
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        device.cmd_copy_image_to_buffer(
            readback.cb,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.staging,
            std::slice::from_ref(&region),
        );
        readback.finish_and_read(ctx, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{tlas_build_draw, ComputeHandles};
    use ash::vk;

    fn dummy_pass() -> ComputeHandles {
        ComputeHandles {
            pipeline: vk::Pipeline::null(),
            layout: vk::PipelineLayout::null(),
            sets: &[],
        }
    }

    /// 空场景（0 实例）不产生 TLAS 组装 dispatch；非空按 64 线程/组向上取整。
    #[test]
    fn tlas_build_skips_empty_scene() {
        assert!(tlas_build_draw(dummy_pass(), 0, 0).is_none());
        let one = tlas_build_draw(dummy_pass(), 1, 0x1000).expect("1 实例必须录 dispatch");
        assert_eq!(one.groups, 1);
        assert_eq!(one.instance_count, 1);
        let sixty_five = tlas_build_draw(dummy_pass(), 65, 0).expect("65 实例必须录 dispatch");
        assert_eq!(sixty_five.groups, 2, "65 线程 = 2 组（64 线程/组）");
    }
}
