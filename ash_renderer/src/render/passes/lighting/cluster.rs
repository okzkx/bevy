//! 光源分簇：把视锥切成 32px tile × 32 深度片的格子，`cluster_build` 每帧算出
//! 每格被哪些点光照到。不透明合成和透明前向都读这份列表。
//!
//! 光源本体在 [`crate::render::LightTable`]，这里只在 b1 引用它。
//! [`cluster_reference`] 是同一算法的 CPU 版，供 BRP `cluster_census` 对照；
//! 改 `cluster_build.wgsl` 的公式时两边要一起改。

use ash::{vk, Device};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::bytes::{put_f32, put_mat4, put_u32};
use crate::common::error::{Result, VulkanError};
use crate::render::core::kit::compute_pipeline::{ComputeHandles, ComputePipeline, Handoff};
use crate::render::core::device::memory::{BufferRole, GpuBuffer};
use crate::render::LightRecord;
use crate::render::Context;
use crate::render::core::kit::storage_sets;

const CLUSTER_BUILD_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cluster_build.spv"));

/// cluster 屏幕 tile 边长（px）。
pub(crate) const CLUSTER_TILE_SIZE: u32 = 32;
/// 深度片数。
pub(crate) const CLUSTER_SLICES: u32 = 32;
/// 每格最多记几盏灯，超出的丢掉（CPU 参照同样丢）。
pub(crate) const MAX_PER_CLUSTER: u32 = 16;
/// 分簇覆盖的视深范围（米）。
pub(crate) const CLUSTER_NEAR: f32 = 0.1;
pub(crate) const CLUSTER_FAR: f32 = 200.0;

/// cluster 均匀数据大小：两个矩阵 128B + 8 个 4B 标量。
pub const CLUSTER_UNIFORM_SIZE: usize = 160;
/// 每格记录大小：count + `MAX_PER_CLUSTER` 个索引，各 4B。
pub const CLUSTER_RECORD_SIZE: usize = 4 * (1 + MAX_PER_CLUSTER as usize);

/// cluster 网格维度，由渲染尺寸算出。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterGrid {
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub slices: u32,
}

impl ClusterGrid {
    /// 格子总数，也是列表与计数表的元素数。
    #[must_use]
    pub fn cluster_count(&self) -> u32 {
        self.tiles_x * self.tiles_y * self.slices
    }
}

/// 屏幕边缘不满一个 tile 的也算一格（与 cluster_build.wgsl 一致）。
#[must_use]
pub(crate) fn cluster_grid(extent: vk::Extent2D) -> ClusterGrid {
    ClusterGrid {
        tiles_x: extent.width.div_ceil(CLUSTER_TILE_SIZE),
        tiles_y: extent.height.div_ceil(CLUSTER_TILE_SIZE),
        slices: CLUSTER_SLICES,
    }
}

/// 第 `slice` 片的视深区间：near·(far/near)^(s/S) 到 near·(far/near)^((s+1)/S)，
/// 近处切得密、远处疏。与 cluster_build.wgsl 同式。
#[must_use]
pub(crate) fn slice_view_depth(slice: u32, grid: ClusterGrid) -> (f32, f32) {
    let ratio = CLUSTER_FAR / CLUSTER_NEAR;
    // 仓库禁用 f32::powf，用 bevy 的 ops::powf
    let z0 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, slice as f32 / grid.slices as f32);
    let z1 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, (slice as f32 + 1.0) / grid.slices as f32);
    (z0, z1)
}

/// 视深 → NDC z（与 cluster_build.wgsl 同式）。透视下 NDC z = a + b/视深，
/// 在屏幕中心取两个已知 NDC z 反投影量出视深，解出 a、b。
/// 坑：不能把 (0,0,−z) 直接乘 view_proj，那是把视空间当世界空间；相机一离开
/// 原点深度片就错位，按 tile 阶梯状漏灯。
#[must_use]
fn slice_ndc_z(view_proj: bevy::math::Mat4, inv_view_proj: bevy::math::Mat4, z_view: f32) -> f32 {
    let depth_at = |ndc_z: f32| {
        let p = inv_view_proj * bevy::math::Vec4::new(0.0, 0.0, ndc_z, 1.0);
        (view_proj * (p.truncate() / p.w).extend(1.0)).w
    };
    let (n0, n1) = (0.5, 0.25);
    let (d0, d1) = (depth_at(n0), depth_at(n1));
    let b = (n0 - n1) / (1.0 / d0 - 1.0 / d1);
    let a = n0 - b / d0;
    a + b / z_view
}

/// 像素 → NDC，y 朝上：顶行对应 +1（与 gbuffer_composite 一致）。
#[must_use]
fn pixel_ndc(px: f32, py: f32, extent: vk::Extent2D) -> (f32, f32) {
    (
        px / extent.width as f32 * 2.0 - 1.0,
        1.0 - 2.0 * py / extent.height as f32,
    )
}

/// CPU 版 cluster_build：每格取 8 个角点反投影成世界 AABB，与光源球求交，
/// 返回每格的光源索引列表。
#[must_use]
pub fn cluster_reference(
    view_proj: bevy::math::Mat4,
    inv_view_proj: bevy::math::Mat4,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    lights: &[LightRecord],
) -> Vec<Vec<u32>> {
    let mut out = Vec::with_capacity(grid.cluster_count() as usize);
    for ci in 0..grid.cluster_count() {
        let slice = ci / (grid.tiles_x * grid.tiles_y);
        let tile = ci % (grid.tiles_x * grid.tiles_y);
        let tx = tile % grid.tiles_x;
        let ty = tile / grid.tiles_x;
        let (z0, z1) = slice_view_depth(slice, grid);
        let (cz0, cz1) =
            (slice_ndc_z(view_proj, inv_view_proj, z0), slice_ndc_z(view_proj, inv_view_proj, z1));
        let (nx0, ny0) = pixel_ndc(
            (tx * CLUSTER_TILE_SIZE) as f32,
            (ty * CLUSTER_TILE_SIZE) as f32,
            extent,
        );
        let (nx1, ny1) = pixel_ndc(
            ((tx + 1) * CLUSTER_TILE_SIZE) as f32,
            ((ty + 1) * CLUSTER_TILE_SIZE) as f32,
            extent,
        );
        let mut min = bevy::math::Vec3::splat(f32::INFINITY);
        let mut max = bevy::math::Vec3::splat(f32::NEG_INFINITY);
        for &(x, y) in &[(nx0, ny0), (nx1, ny0), (nx0, ny1), (nx1, ny1)] {
            for &cz in &[cz0, cz1] {
                let clip = inv_view_proj * bevy::math::Vec4::new(x, y, cz, 1.0);
                let world = clip.truncate() / clip.w;
                min = min.min(world);
                max = max.max(world);
            }
        }
        let mut list = Vec::new();
        for (l, light) in lights.iter().enumerate() {
            let c = bevy::math::Vec3::from_slice(&light.pos_radius[..3]);
            let r = light.pos_radius[3];
            // 已回收的空槽半径为 0，跳过（GPU 同）
            if r <= 0.0 {
                continue;
            }
            let closest = c.clamp(min, max);
            if closest.distance(c) <= r {
                list.push(l as u32);
                if list.len() == MAX_PER_CLUSTER as usize {
                    break;
                }
            }
        }
        out.push(list);
    }
    out
}

/// 打包 cluster 均匀数据。偏移必须与 WGSL `ClusterParams` 一致：view_proj @0、
/// inv_view_proj @64、near/far/tiles_x/tiles_y/slices/light_count/extent @128 起。
#[must_use]
pub fn pack_cluster_uniforms(
    view_proj: &bevy::math::Mat4,
    inv_view_proj: &bevy::math::Mat4,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    light_count: u32,
) -> [u8; CLUSTER_UNIFORM_SIZE] {
    let mut out = [0u8; CLUSTER_UNIFORM_SIZE];
    put_mat4(&mut out, 0, view_proj);
    put_mat4(&mut out, 64, inv_view_proj);
    put_f32(&mut out, 128, CLUSTER_NEAR);
    put_f32(&mut out, 132, CLUSTER_FAR);
    put_u32(&mut out, 136, grid.tiles_x);
    put_u32(&mut out, 140, grid.tiles_y);
    put_u32(&mut out, 144, grid.slices);
    put_u32(&mut out, 148, light_count);
    put_f32(&mut out, 152, extent.width as f32);
    put_f32(&mut out, 156, extent.height as f32);
    out
}

/// 光源分簇资源，每帧槽一份。set 布局：b0 均匀数据 / b1 光源常驻表 / b2 每格
/// 列表 / b3 每格计数。cluster_build 写，合成与透明前向读，透明收集只读 b0。
#[derive(Resource)]
pub struct LightClusters {
    device: Device,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 每帧槽一个，resize 时随列表一起重建。
    sets: Vec<vk::DescriptorSet>,
    uniforms: Vec<GpuBuffer>,
    /// 每格光源列表（显存）。
    lists: Vec<GpuBuffer>,
    /// 每格光源数（CPU 可见，BRP 直读）。
    counts: Vec<GpuBuffer>,
}

impl LightClusters {
    /// 建到一半失败时，已建的部分由 Drop 回收。
    ///
    /// # Errors
    /// buffer 创建/内存分配/描述符对象创建失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D, light_buffers: &[vk::Buffer]) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            extent: vk::Extent2D::default(),
            grid: cluster_grid(extent),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            uniforms: Vec::new(),
            lists: Vec::new(),
            counts: Vec::new(),
        };
        this.set_layout = storage_sets::create_layout::<4>(
            &ctx.device,
            vk::ShaderStageFlags::COMPUTE | vk::ShaderStageFlags::FRAGMENT,
        )?;
        this.build(ctx, light_buffers)?;
        this.extent = extent;
        info!(
            "光源分簇资源就绪: cluster 网格 {}×{}×{}（{} clusters，tile {CLUSTER_TILE_SIZE}px）\
             × {} 帧槽；光源 = 常驻灯表(b1 绑定)、单 cluster 上限 {MAX_PER_CLUSTER}",
            this.grid.tiles_x,
            this.grid.tiles_y,
            this.grid.slices,
            this.grid.cluster_count(),
            this.sets.len(),
        );
        Ok(this)
    }

    /// 按当前 `grid` 建 buffer 和描述符集；调用前旧的必须已经 drop_resources。
    fn build(&mut self, ctx: &Context, light_buffers: &[vk::Buffer]) -> Result<()> {
        let sharing = ctx.sharing_families();
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(&ctx.device, ctx.memory_contract(), size, role, &sharing)
        };
        let clusters = u64::from(self.grid.cluster_count());
        let mut slots = Vec::with_capacity(slot_count());
        for slot in 0..slot_count() {
            // b1 只引用 LightTable 的帧槽 buffer，不归这里销毁
            let light = *light_buffers.get(slot % light_buffers.len().max(1)).ok_or_else(|| {
                VulkanError::Init("光源常驻表帧槽环数量少于 cluster 帧槽".into())
            })?;
            self.uniforms.push(mk(BufferRole::Storage, CLUSTER_UNIFORM_SIZE as u64)?);
            self.lists.push(mk(BufferRole::DeviceStorage, clusters * CLUSTER_RECORD_SIZE as u64)?);
            self.counts.push(mk(BufferRole::Storage, clusters * 4)?);
            slots.push([
                self.uniforms[slot].buffer(),
                light,
                self.lists[slot].buffer(),
                self.counts[slot].buffer(),
            ]);
        }
        (self.pool, self.sets) = unsafe { storage_sets::create_sets(&ctx.device, self.set_layout, &slots) }?;
        Ok(())
    }

    #[must_use]
    pub fn grid(&self) -> ClusterGrid {
        self.grid
    }

    /// 当前网格对应的渲染尺寸（BRP 对照用）。
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    #[must_use]
    pub fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    #[must_use]
    pub fn frame_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot % self.sets.len()]
    }

    /// 第 `slot` 帧槽的均匀数据，CPU 每帧写。
    #[must_use]
    pub fn uniform(&mut self, slot: usize) -> &mut GpuBuffer {
        let n = self.uniforms.len();
        &mut self.uniforms[slot % n]
    }

    /// 第 `slot` 帧槽的每格计数，BRP 直读。
    #[must_use]
    pub fn counts(&mut self, slot: usize) -> &mut GpuBuffer {
        let n = self.counts.len();
        &mut self.counts[slot % n]
    }

    /// 尺寸没变直接返回；变了先等 GPU 空闲，再按新网格重建 buffer 和描述符集。
    /// set 布局与尺寸无关，不动。
    ///
    /// # Errors
    /// 等待排空或新 buffer/描述符创建失败。
    pub fn rebuild(
        &mut self,
        ctx: &Context,
        extent: vk::Extent2D,
        light_buffers: &[vk::Buffer],
    ) -> Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        self.drop_resources();
        self.grid = cluster_grid(extent);
        self.build(ctx, light_buffers)?;
        self.extent = extent;
        info!(
            "光源分簇资源随 swapchain 重建: cluster 网格 {}×{}×{}（{} clusters）",
            self.grid.tiles_x,
            self.grid.tiles_y,
            self.grid.slices,
            self.grid.cluster_count(),
        );
        Ok(())
    }

    fn drop_resources(&mut self) {
        if self.pool != vk::DescriptorPool::null() {
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
            self.pool = vk::DescriptorPool::null();
        }
        self.sets.clear();
        self.uniforms.clear();
        self.lists.clear();
        self.counts.clear();
    }
}

fn slot_count() -> usize {
    crate::render::MAX_FRAMES_IN_FLIGHT
}

impl Drop for LightClusters {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown 已先 device_wait_idle
        self.drop_resources();
        if self.set_layout != vk::DescriptorSetLayout::null() {
            unsafe { self.device.destroy_descriptor_set_layout(self.set_layout, None) };
        }
    }
}

/// cluster_build 的一次 dispatch。
#[derive(Clone, Copy)]
pub struct ClusterBuildDraw {
    pub pass: ComputeHandles,
    pub groups: u32,
}

/// cluster_build compute 管线：只绑光源分簇集，没有 push（参数都在 b0）。
#[derive(Resource)]
pub struct ClusterBuildPass {
    pipeline: ComputePipeline,
}

impl ClusterBuildPass {
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, cluster_layout: vk::DescriptorSetLayout) -> Result<Self> {
        let pipeline =
            ComputePipeline::new(&ctx.device, CLUSTER_BUILD_SPV, &[cluster_layout], 0, c"cs_main")?;
        info!("cluster_build compute 就绪（光源分簇单集）");
        Ok(Self { pipeline })
    }

    /// 每线程组 64 格。
    #[must_use]
    pub fn draw(&self, grid: ClusterGrid) -> ClusterBuildDraw {
        ClusterBuildDraw {
            pass: self.pipeline.handles(),
            groups: grid.cluster_count().div_ceil(64),
        }
    }
}

/// 录制 cluster_build；自带的屏障保证后面的合成与透明片元着色器读到新列表。
pub(crate) fn record_cluster_build(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    cluster_set: vk::DescriptorSet,
    draw: &ClusterBuildDraw,
) {
    draw.pass.record(device, command_buffer, &[cluster_set], &[], draw.groups, Handoff::ComputeFragmentRead);
}

#[cfg(test)]
mod tests {
    use super::slice_ndc_z;
    use bevy::math::{Mat4, Vec3};

    /// 相机不在原点时，算出的 NDC z 要等于视深 z 处真实点的 NDC z。
    #[test]
    #[expect(deprecated, reason = "基准矩阵沿用退役构造器：bevy_math 只重导出 proj 族，view 族的 look_to_mat4 不在出口内，本测试不改写投影语义")]
    fn slice_ndc_z_matches_off_origin_camera() {
        let eye = Vec3::new(5.0, 2.0, 1.0);
        let forward = Vec3::new(-1.0, -0.1, 0.0).normalize();
        let view = Mat4::look_to_rh(eye, forward, Vec3::Y);
        let proj = Mat4::perspective_infinite_reverse_rh(1.0, 16.0 / 9.0, 0.1);
        let view_proj = proj * view;
        let inv_view_proj = view_proj.inverse();
        for z in [0.5_f32, 3.0, 20.0, 150.0] {
            let p = view_proj * (eye + forward * z).extend(1.0);
            let expected = p.z / p.w;
            let got = slice_ndc_z(view_proj, inv_view_proj, z);
            assert!((got - expected).abs() < 1e-5, "z={z}: got {got}, expected {expected}");
        }
    }
}
