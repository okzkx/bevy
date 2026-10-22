//! 透明 Clustered Forward 层（8.1.3/8.1.4）：3D cluster 光源分簇 + 透明命令
//! GPU 组装 + 透明前向绘制管线。
//!
//! 数据流（全部 GPU 侧决策——GPU-driven 红线：透明行的筛选/排序/命令组装
//! 都在 compute，CPU 只发在账上界）：
//! 1. `cluster_build`（compute）：视锥按 32px tile × 32 深度片分簇（**开工即
//!    3D cluster**，用户 2026-10-20 拍板），逐 cluster 以世界空间 AABB（
//!    inv_view_proj 投 8 角点）与点光球求交，写光源索引表（DEVICE_LOCAL）+
//!    逐 cluster 计数（HOST_VISIBLE——`ash_renderer/cluster_census` 逐 tile
//!    对照的读数面，判定线②）。
//! 2. `transparent_collect`（compute）：从在场环筛材质 alpha<1 的行，atomicAdd
//!    紧凑成"透明行索引 + 深度键"列表——深度键是 8.1.4 bitonic 排序的输入，
//!    本段先按在场序画（物体级排序归 8.1.4）。
//! 3. `transparent_gather`（compute）：索引列表 → 透明命令环（与 5.3 命令同
//!    20B 布局，LOD 引脚选择同 build_indirect 语义）。
//! 4. 透明前向绘制：`cmd_draw_indexed_indirect`（count = 在场行数上界，非
//!    透明行 instanceCount=0），alpha blending 落 HDR 中间目标；不透明深度对
//!    透明的复用形态 = **附件重绑**（施工钉明：6.1 已实证深度附件不可采样，
//!    取数通道要多一张取数目标+拷贝，附件重绑免拷贝零带宽——透明 pass 把
//!    G-buffer 深度再挂回 depth attachment，loadOp LOAD、只测不写）。
//!
//! 光源列表 = 每帧环（HOST_VISIBLE，宿主每帧重写）：8.2 才裁决"常驻表 vs
//! 每帧环"的光源驻留形态——本段的环是透明层的测试载体（场景无点光源实体，
//! 测试光源经 `ash_renderer/cluster_lights` 注入），不预设 8.2 的裁决。
//!
//! CPU 参照（[`cluster_reference`] 纯函数 + 单测）：与 cluster_build.wgsl
//! 同公式（分片边界/AABB 投影/球-AABB 求交），供 BRP census 对照。

use ash::{vk, Device};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::error::{Result, VulkanError};
use crate::render::resources::{BufferRole, GpuBuffer};
use crate::render::Context;

const CLUSTER_BUILD_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cluster_build.spv"));
const TRANSPARENT_COLLECT_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/transparent_collect.spv"));
const TRANSPARENT_GATHER_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/transparent_gather.spv"));
const TRANSPARENT_SORT_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/transparent_sort.spv"));
const TRANSPARENT_FORWARD_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/transparent_forward.spv"));

/// cluster 屏幕 tile 边长（px）。
pub const CLUSTER_TILE_SIZE: u32 = 32;
/// 深度分片数（视图深度指数划分）。
pub const CLUSTER_SLICES: u32 = 32;
/// 点光源列表容量（每帧环；8.2 光源扩容前的测试载体）。
pub const MAX_CLUSTER_LIGHTS: u32 = 64;
/// 单 cluster 光源索引容量（超出截断——CPU 参照同语义，计数面照实对照）。
pub const MAX_PER_CLUSTER: u32 = 16;
/// 深度分片区间（视图深度，米）。
pub const CLUSTER_NEAR: f32 = 0.1;
pub const CLUSTER_FAR: f32 = 200.0;
/// 透明行列表容量（索引/键/命令三环同容量；在场行数作上界在此截断并在
/// 诊断读数显式报——与对象表容量纪律同族，学习项目规模下不预期触顶）。
pub const TRANSPARENT_CAPACITY: u32 = 16384;

/// 点光源 GPU 记录步长（2×vec4 = 32B，与 WGSL `PointLight` 同序互证）。
pub const POINT_LIGHT_STRIDE: usize = 32;
/// cluster 均匀数据步长：view_proj 64B + inv_view_proj 64B + 标量 8×4B + 垫 16B。
pub const CLUSTER_UNIFORM_SIZE: usize = 160;
/// cluster 记录步长（count + 16 索引）。
pub const CLUSTER_RECORD_SIZE: usize = 4 * (1 + MAX_PER_CLUSTER as usize);

/// 透明族独立小集的 binding 编号（hiz 先例：不进 set2）。
pub const CLUSTER_BINDING_UNIFORM: u32 = 0;
pub const CLUSTER_BINDING_LIGHTS: u32 = 1;
pub const CLUSTER_BINDING_LISTS: u32 = 2;
pub const CLUSTER_BINDING_COUNTS: u32 = 3;
pub const CLUSTER_BINDING_INDEX_LIST: u32 = 4;
pub const CLUSTER_BINDING_KEYS: u32 = 5;
pub const CLUSTER_BINDING_COMMANDS: u32 = 6;
pub const CLUSTER_BINDING_COUNT: u32 = 7;

/// 一盏测试点光源（BRP 注入 → 光源环；8.2 前的透明层光照数据载体）。
#[derive(Clone, Copy, Debug)]
pub struct PointLightGpu {
    /// 世界空间位置（xyz）+ 作用半径（w，米）。
    pub pos_radius: [f32; 4],
    /// 颜色 × 强度（rgb，线性；w 未用）。
    pub color_intensity: [f32; 4],
}

impl PointLightGpu {
    /// 打包 32B 进光源环（与 WGSL `PointLight` 同序互证）。
    #[must_use]
    pub fn pack(&self) -> [u8; POINT_LIGHT_STRIDE] {
        let mut out = [0u8; POINT_LIGHT_STRIDE];
        for (i, v) in self
            .pos_radius
            .iter()
            .chain(self.color_intensity.iter())
            .enumerate()
        {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// BRP 注入的测试点光源列表（draw_frame 每帧写光源环的来源；空 = 纯方向光
/// 语义不变）。只喂透明层 cluster——不透明合成仍单方向光（8.2 多光源归
/// 光照累积 pass）。
#[derive(Resource, Default)]
pub struct TestPointLights(pub Vec<PointLightGpu>);

/// cluster 网格维度（由 extent 推得，重建时更新）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterGrid {
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub slices: u32,
}

impl ClusterGrid {
    /// 当前 cluster 总数（= 计数表/索引表元素数）。
    #[must_use]
    pub fn cluster_count(&self) -> u32 {
        self.tiles_x * self.tiles_y * self.slices
    }
}

/// 由 extent 推 cluster 网格（tile 取整到屏幕边缘，最后一块不满 tile 也成簇
/// ——cluster_build 的像素→tile 映射同式）。
#[must_use]
pub fn cluster_grid(extent: vk::Extent2D) -> ClusterGrid {
    ClusterGrid {
        tiles_x: extent.width.div_ceil(CLUSTER_TILE_SIZE),
        tiles_y: extent.height.div_ceil(CLUSTER_TILE_SIZE),
        slices: CLUSTER_SLICES,
    }
}

/// 深度分片边界（视图深度）：第 s 片覆盖
/// [near·(far/near)^(s/S), near·(far/near)^((s+1)/S))——指数分片（近密远疏，
/// 点光源作用域集中在近中景的行业默认）。CPU 参照与 GPU 同公式。
#[must_use]
pub fn slice_view_depth(slice: u32, grid: ClusterGrid) -> (f32, f32) {
    let ratio = CLUSTER_FAR / CLUSTER_NEAR;
    // libm powf（bevy_math::ops——仓库 disallowed-methods 纪律；WGSL 端 pow 同式）
    let z0 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, slice as f32 / grid.slices as f32);
    let z1 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, (slice as f32 + 1.0) / grid.slices as f32);
    (z0, z1)
}

/// 视图深度 → NDC z（与 GPU 同式：clip = view_proj × (0,0,−z,1) 取 z/w）。
/// bevy 视图空间右手系（相机看 −z），距离取 −z 进投影。
#[must_use]
fn slice_ndc_z(view_proj: bevy::math::Mat4, z_view: f32) -> f32 {
    let clip = view_proj * bevy::math::Vec4::new(0.0, 0.0, -z_view, 1.0);
    clip.z / clip.w
}

/// 像素 → shader-NDC（y 翻转约定与 gbuffer_composite 同源：顶行 ↔ +1）。
#[must_use]
fn pixel_ndc(px: f32, py: f32, extent: vk::Extent2D) -> (f32, f32) {
    (
        px / extent.width as f32 * 2.0 - 1.0,
        1.0 - 2.0 * py / extent.height as f32,
    )
}

/// CPU 参照：逐 cluster 的光源索引集合（判定线②的对照真值）。与
/// cluster_build.wgsl 同公式——cluster 世界 AABB（tile×深度片 8 角点经
/// inv_view_proj 反投影）与光源球求交；单 cluster 超 [`MAX_PER_CLUSTER`]
/// 截断（与 GPU 同语义）。
#[must_use]
pub fn cluster_reference(
    view_proj: bevy::math::Mat4,
    inv_view_proj: bevy::math::Mat4,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    lights: &[PointLightGpu],
) -> Vec<Vec<u32>> {
    let mut out = Vec::with_capacity(grid.cluster_count() as usize);
    for ci in 0..grid.cluster_count() {
        let slice = ci / (grid.tiles_x * grid.tiles_y);
        let tile = ci % (grid.tiles_x * grid.tiles_y);
        let tx = tile % grid.tiles_x;
        let ty = tile / grid.tiles_x;
        let (z0, z1) = slice_view_depth(slice, grid);
        let (cz0, cz1) = (slice_ndc_z(view_proj, z0), slice_ndc_z(view_proj, z1));
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
            let closest = c.clamp(min, max);
            if closest.distance(c) <= r {
                list.push(l as u32);
                if list.len() == MAX_PER_CLUSTER as usize {
                    break; // 截断语义与 GPU 同
                }
            }
        }
        out.push(list);
    }
    out
}

/// cluster 均匀数据打包（160B，与 WGSL `ClusterParams` 偏移同源）：
/// view_proj @0、inv_view_proj @64、near/far/tiles_x/tiles_y/slices/light_count @128..。
#[must_use]
pub fn pack_cluster_uniforms(
    view_proj: &bevy::math::Mat4,
    inv_view_proj: &bevy::math::Mat4,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    light_count: u32,
) -> [u8; CLUSTER_UNIFORM_SIZE] {
    let mut out = [0u8; CLUSTER_UNIFORM_SIZE];
    let put_f32 = |out: &mut [u8], off: usize, v: f32| {
        out[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let put_u32 = |out: &mut [u8], off: usize, v: u32| {
        out[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    for (i, v) in view_proj.to_cols_array().iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    for (i, v) in inv_view_proj.to_cols_array().iter().enumerate() {
        out[64 + i * 4..64 + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
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

/// 透明族全量资源：光源环/cluster 表/透明命令链（×`MAX_FRAMES_IN_FLIGHT`
/// 帧槽）+ 独立小集（b0..b7）。
#[derive(Resource)]
pub struct ClusterTargets {
    device: Device,
    extent: vk::Extent2D,
    grid: ClusterGrid,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 帧槽一一对应（描述符指向各槽 buffer，句柄稳定——resize 只换
    /// cluster 表本体并重写描述符）。
    sets: Vec<vk::DescriptorSet>,
    uniforms: Vec<GpuBuffer>,
    /// 光源环（HOST_VISIBLE，宿主每帧重写）。
    lights_rings: Vec<GpuBuffer>,
    /// cluster 索引表（DEVICE_LOCAL，cluster_build 写 / 透明 FS 读）。
    cluster_lists: Vec<GpuBuffer>,
    /// 逐 cluster 计数（HOST_VISIBLE，cluster_build 写，BRP 读数面）。
    cluster_counts: Vec<GpuBuffer>,
    /// 透明行索引列表（DEVICE_LOCAL，collect 紧凑写 / gather 读）。
    t_index_lists: Vec<GpuBuffer>,
    /// 深度键（f32 视图深度，8.1.4 bitonic 排序输入）。
    t_keys: Vec<GpuBuffer>,
    /// 透明命令环（INDIRECT 用途，gather 写 / draw 读）。
    t_commands: Vec<GpuBuffer>,
    /// 透明行计数（HOST_VISIBLE storage：宿主每帧清零 + collect 原子累加 +
    /// BRP 直读——三方的共同载体）。
    t_counts: Vec<GpuBuffer>,
}

impl ClusterTargets {
    /// 建资源链：逐槽 buffer → set 布局（b0..b7）→ pool + 描述符写入。
    /// 半建失败逐层回收（全有或全无）。
    ///
    /// # Errors
    /// buffer 创建/内存分配/描述符对象创建失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D) -> Result<Self> {
        let grid = cluster_grid(extent);
        let mut this = Self {
            device: ctx.device.clone(),
            extent: vk::Extent2D::default(),
            grid,
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            uniforms: Vec::new(),
            lights_rings: Vec::new(),
            cluster_lists: Vec::new(),
            cluster_counts: Vec::new(),
            t_index_lists: Vec::new(),
            t_keys: Vec::new(),
            t_commands: Vec::new(),
            t_counts: Vec::new(),
        };
        let slots = slot_count();
        this.build_buffers(ctx, slots)?;
        this.build_descriptors(ctx, slots)?;
        this.extent = extent;
        info!(
            "透明层资源就绪: cluster 网格 {}×{}×{}（{} clusters，tile {CLUSTER_TILE_SIZE}px）\
             × {} 帧槽；光源环 {MAX_CLUSTER_LIGHTS}、单 cluster 上限 {MAX_PER_CLUSTER}、\
             透明行容量 {TRANSPARENT_CAPACITY}（命令环 + 索引/键列表；排序归 8.1.4）",
            grid.tiles_x,
            grid.tiles_y,
            grid.slices,
            grid.cluster_count(),
            slots,
        );
        Ok(this)
    }

    fn build_buffers(&mut self, ctx: &Context, slots: usize) -> Result<()> {
        let sharing: Vec<u32> = if ctx.transfer_queue_family_index != ctx.queue_family_index {
            vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
        } else {
            Vec::new()
        };
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(&ctx.device, ctx.memory_contract(), size, role, &sharing)
        };
        for _ in 0..slots {
            self.uniforms.push(mk(BufferRole::Storage, CLUSTER_UNIFORM_SIZE as u64)?);
            self.lights_rings
                .push(mk(BufferRole::Storage, u64::from(MAX_CLUSTER_LIGHTS) * POINT_LIGHT_STRIDE as u64)?);
            self.cluster_counts.push(mk(BufferRole::Storage, u64::from(self.grid.cluster_count()) * 4)?);
            self.t_counts.push(mk(BufferRole::Storage, 4)?);
            self.cluster_lists.push(mk(
                BufferRole::DeviceStorage,
                u64::from(self.grid.cluster_count()) * CLUSTER_RECORD_SIZE as u64,
            )?);
            self.t_index_lists
                .push(mk(BufferRole::DeviceStorage, u64::from(TRANSPARENT_CAPACITY) * 4)?);
            self.t_keys
                .push(mk(BufferRole::DeviceStorage, u64::from(TRANSPARENT_CAPACITY) * 4)?);
            self.t_commands.push(mk(
                BufferRole::IndirectCommands,
                u64::from(TRANSPARENT_CAPACITY) * 20,
            )?);
        }
        Ok(())
    }

    fn build_descriptors(&mut self, ctx: &Context, slots: usize) -> Result<()> {
        let device = &ctx.device;
        let stages = vk::ShaderStageFlags::COMPUTE | vk::ShaderStageFlags::FRAGMENT;
        let storage = |binding: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(stages)
        };
        let bindings = [
            storage(CLUSTER_BINDING_UNIFORM),
            storage(CLUSTER_BINDING_LIGHTS),
            storage(CLUSTER_BINDING_LISTS),
            storage(CLUSTER_BINDING_COUNTS),
            storage(CLUSTER_BINDING_INDEX_LIST),
            storage(CLUSTER_BINDING_KEYS),
            storage(CLUSTER_BINDING_COMMANDS),
            storage(CLUSTER_BINDING_COUNT),
        ];
        self.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let result = unsafe { self.build_descriptors_inner(ctx, slots) };
        if result.is_err() {
            unsafe { device.destroy_descriptor_set_layout(self.set_layout, None) };
            self.set_layout = vk::DescriptorSetLayout::null();
        }
        result
    }

    /// # Safety
    /// `set_layout` 须已创建成功。
    unsafe fn build_descriptors_inner(&mut self, ctx: &Context, slots: usize) -> Result<()> {
        unsafe {
            let device = &ctx.device;
            self.pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(slots as u32)
                    .pool_sizes(&[vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(8 * slots as u32)]),
                None,
            )?;
            let sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(
                        &std::iter::repeat_n(self.set_layout, slots).collect::<Vec<_>>(),
                    ),
            );
            self.sets = match sets {
                Ok(s) => s,
                Err(e) => return Err(VulkanError::from(e)),
            };
            let mut infos = Vec::with_capacity(8 * slots);
            for slot in 0..slots {
                let groups: [&GpuBuffer; 8] = [
                    &self.uniforms[slot],
                    &self.lights_rings[slot],
                    &self.cluster_lists[slot],
                    &self.cluster_counts[slot],
                    &self.t_index_lists[slot],
                    &self.t_keys[slot],
                    &self.t_commands[slot],
                    &self.t_counts[slot],
                ];
                for buffer in groups {
                    infos.push(
                        vk::DescriptorBufferInfo::default()
                            .buffer(buffer.buffer())
                            .offset(0)
                            .range(vk::WHOLE_SIZE),
                    );
                }
            }
            let bindings = [
                CLUSTER_BINDING_UNIFORM,
                CLUSTER_BINDING_LIGHTS,
                CLUSTER_BINDING_LISTS,
                CLUSTER_BINDING_COUNTS,
                CLUSTER_BINDING_INDEX_LIST,
                CLUSTER_BINDING_KEYS,
                CLUSTER_BINDING_COMMANDS,
                CLUSTER_BINDING_COUNT,
            ];
            let writes: Vec<vk::WriteDescriptorSet> = self
                .sets
                .iter()
                .zip(infos.chunks(8))
                .flat_map(|(set, infos)| {
                    bindings.iter().zip(infos).map(|(binding, info)| {
                        vk::WriteDescriptorSet::default()
                            .dst_set(*set)
                            .dst_binding(*binding)
                            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                            .buffer_info(std::slice::from_ref(info))
                    })
                })
                .collect();
            device.update_descriptor_sets(&writes, &[]);
            Ok(())
        }
    }

    /// cluster 网格维度。
    #[must_use]
    pub fn grid(&self) -> ClusterGrid {
        self.grid
    }

    /// 当前渲染目标尺寸（BRP 对照的口径口）。
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 透明族 set 布局（compute [set2, cluster_set] 与透明图形管线第 4 集的
    /// layout 素材）。
    #[must_use]
    pub fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    /// 第 `slot` 帧槽的透明族集。
    #[must_use]
    pub fn frame_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot % self.sets.len()]
    }

    /// 第 `slot` 帧槽的 cluster 均匀数据 buffer（宿主写入口）。
    #[must_use]
    pub fn uniform(&mut self, slot: usize) -> &mut GpuBuffer {
        { let n = self.uniforms.len(); &mut self.uniforms[slot % n] }
    }

    /// 第 `slot` 帧槽的光源环（宿主写入口）。
    #[must_use]
    pub fn lights_ring(&mut self, slot: usize) -> &mut GpuBuffer {
        { let n = self.lights_rings.len(); &mut self.lights_rings[slot % n] }
    }

    /// 第 `slot` 帧槽的透明命令环句柄（draw 消费）。
    #[must_use]
    pub fn t_command_buffer(&self, slot: usize) -> vk::Buffer {
        self.t_commands[slot % self.t_commands.len()].buffer()
    }

    /// 第 `slot` 帧槽的深度键 buffer 句柄（帧首 -inf fill 的目标，8.1.4）。
    #[must_use]
    pub fn t_key_buffer(&self, slot: usize) -> vk::Buffer {
        self.t_keys[slot % self.t_keys.len()].buffer()
    }

    /// 第 `slot` 帧槽的透明索引/键列表回读（判定线③对照面；诊断一次性
    /// staging，低频 BRP 调用）。
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
        crate::render::gpu::hiz::readback_buffer(
            ctx,
            self.t_index_lists[slot % self.t_index_lists.len()].buffer(),
            indices,
        )?;
        crate::render::gpu::hiz::readback_buffer(
            ctx,
            self.t_keys[slot % self.t_keys.len()].buffer(),
            keys,
        )
    }

    /// 第 `slot` 帧槽的透明计数（BRP 直读面：本帧槽 = 两帧前输出，fence 纪律）。
    #[must_use]
    pub fn t_count(&mut self, slot: usize) -> &mut GpuBuffer {
        { let n = self.t_counts.len(); &mut self.t_counts[slot % n] }
    }

    /// 第 `slot` 帧槽的逐 cluster 计数（BRP 直读面）。
    #[must_use]
    pub fn cluster_counts(&mut self, slot: usize) -> &mut GpuBuffer {
        { let n = self.cluster_counts.len(); &mut self.cluster_counts[slot % n] }
    }

    /// 第 `slot` 帧槽的透明行索引列表回读（判定线③对照面；诊断提交走
    /// hiz readback 同款一次性 staging——调用方为低频 BRP，不在帧循环内）。
    ///
    /// # Errors
    /// 读回提交失败（设备丢失等）。
    pub fn readback_index_list(&self, ctx: &Context, slot: usize, bytes: &mut [u8]) -> Result<()> {
        crate::render::gpu::hiz::readback_buffer(
            ctx,
            self.t_index_lists[slot % self.t_index_lists.len()].buffer(),
            bytes,
        )
    }

    /// 随 swapchain 尺寸重建（幂等：extent 没变就空手而归）。cluster 表随
    /// 网格重建（buffer 重建 + 描述符重写）；透明命令链与尺寸无关不动。
    ///
    /// # Errors
    /// 等待排空或新 buffer/描述符创建失败。
    pub fn rebuild(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        let slots = self.sets.len();
        self.drop_resources();
        self.grid = cluster_grid(extent);
        self.build_buffers(ctx, slots)?;
        self.build_descriptors(ctx, slots)?;
        self.extent = extent;
        info!(
            "透明层资源随 swapchain 重建: cluster 网格 {}×{}×{}（{} clusters）",
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
        for v in [
            &mut self.uniforms,
            &mut self.lights_rings,
            &mut self.cluster_lists,
            &mut self.cluster_counts,
            &mut self.t_index_lists,
            &mut self.t_keys,
            &mut self.t_commands,
            &mut self.t_counts,
        ] {
            v.clear();
        }
    }
}

fn slot_count() -> usize {
    crate::render::MAX_FRAMES_IN_FLIGHT
}

impl Drop for ClusterTargets {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown 的 device_wait_idle 先行
        self.drop_resources();
        if self.set_layout != vk::DescriptorSetLayout::null() {
            unsafe { self.device.destroy_descriptor_set_layout(self.set_layout, None) };
        }
    }
}

// ============ compute 半边（build / collect / gather） ============

// compute push 总长（collect/gather 共用形状）：4B 在场行数上界。
pub const TRANSPARENT_PUSH_SIZE: u32 = 4;

// ============ 帧录制输入束 ============

/// 一帧透明层的录制输入（draw_frame 组装，`record_frame` 消费）。
pub struct TransparentFrame {
    /// cluster 构建 dispatch（Some = 有 cluster 可建——空场仍建以清零计数面？
    /// 否：空场不派，计数面无读者，诊断读数标注帧槽陈旧度即可）。
    pub cluster_build: Option<ClusterBuildDraw>,
    pub cluster_set: vk::DescriptorSet,
    /// 透明行收集 + 命令组装 dispatch（count = 在场行数 > 0 才 Some）。
    pub collect: Option<TransparentCollectDraw>,
    pub gather: Option<TransparentGatherDraw>,
    /// bitonic 排序计划（8.1.4；prev 透明计数 = 0 的让路帧 None——数组未排，
    /// gather 尾部反向读取仍覆盖全部真实行，只丢一帧排序）。
    pub sort: Option<TransparentSortPlan>,
    /// set2（常驻表，gather 读；透明绘制也绑）。
    pub set2: vk::DescriptorSet,
    /// 透明前向绘制管线（layout 四集）。
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 本帧槽透明命令环（`command_count` 上界内有效）。
    pub t_commands: vk::Buffer,
    /// 本帧槽深度键 buffer（帧首 -inf fill 目标，8.1.4）。
    pub t_keys: vk::Buffer,
    /// 命令上界 = 在场行数（CPU 在账；非透明行 instanceCount=0）。
    pub command_count: u32,
}

/// cluster 构建 dispatch 输入（组数 = cluster 总数取整到 workgroup 64）。
#[derive(Clone, Copy)]
pub struct ClusterBuildDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub groups: u32,
}

/// 透明行收集 dispatch 输入（push = 在场行数上界）。
#[derive(Clone, Copy)]
pub struct TransparentCollectDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// 透明命令组装 dispatch 输入（push = 在场行数上界，guard 用）。
#[derive(Clone, Copy)]
pub struct TransparentGatherDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// bitonic 排序 push 步长：(k, j) 段参数 = 8B。
pub const TRANSPARENT_SORT_PUSH: u32 = 8;

/// bitonic 排序 dispatch 输入（管线 + 组数 + 预生成 (k,j) 派发序列）。
pub struct TransparentSortPlan {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 每派发组数 = CAPACITY/2 取整到 workgroup（容量恒定，序列恒定）。
    pub groups: u32,
    /// (k, j) 段参数序列（建管线时预生成，恒 105 项；逐帧 clone 840B——
    /// 生命周期独立于 TransparentPasses，FrameDraw 组装免借用纠缠）。
    pub passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
}

/// compute 半边三件：build（[cluster_set] 单集）/ collect + gather（[set2,
/// cluster_set] 双集）——与 hiz/occlusion 的双集先例同形状。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPasses {
    device: Device,
    sort_layout: vk::PipelineLayout,
    sort_pipeline: vk::Pipeline,
    /// bitonic (k, j) 派发序列（容量恒定 → 建成时一次生成）。
    sort_passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
    build_layout: vk::PipelineLayout,
    build_pipeline: vk::Pipeline,
    collect_layout: vk::PipelineLayout,
    collect_pipeline: vk::Pipeline,
    gather_layout: vk::PipelineLayout,
    gather_pipeline: vk::Pipeline,
}

impl TransparentPasses {
    /// 建三管线链：shader module → layout → ComputePipeline。半建失败逆序回收
    ///（全有或全无）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        cluster_set_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            sort_layout: vk::PipelineLayout::null(),
            sort_pipeline: vk::Pipeline::null(),
            sort_passes: bitonic_sequence(TRANSPARENT_CAPACITY),
            build_layout: vk::PipelineLayout::null(),
            build_pipeline: vk::Pipeline::null(),
            collect_layout: vk::PipelineLayout::null(),
            collect_pipeline: vk::Pipeline::null(),
            gather_layout: vk::PipelineLayout::null(),
            gather_pipeline: vk::Pipeline::null(),
        };
        let result: Result<()> = (|| {
            // cluster_build：单集、无 push（参数全在 uniform buffer）
            let (build_pipeline, build_layout) =
                make_compute(ctx, CLUSTER_BUILD_SPV, &[cluster_set_layout], 0)?;
            this.build_pipeline = build_pipeline;
            this.build_layout = build_layout;
            let (sort_pipeline, sort_layout) =
                make_compute(ctx, TRANSPARENT_SORT_SPV, &[cluster_set_layout], TRANSPARENT_SORT_PUSH)?;
            this.sort_pipeline = sort_pipeline;
            this.sort_layout = sort_layout;
            let (collect_pipeline, collect_layout) = make_compute(
                ctx,
                TRANSPARENT_COLLECT_SPV,
                &[set2_layout, cluster_set_layout],
                TRANSPARENT_PUSH_SIZE,
            )?;
            this.collect_pipeline = collect_pipeline;
            this.collect_layout = collect_layout;
            let (gather_pipeline, gather_layout) = make_compute(
                ctx,
                TRANSPARENT_GATHER_SPV,
                &[set2_layout, cluster_set_layout],
                TRANSPARENT_PUSH_SIZE,
            )?;
            this.gather_pipeline = gather_pipeline;
            this.gather_layout = gather_layout;
            Ok(())
        })();
        if result.is_err() {
            this.drop_pipelines();
        }
        result?;
        info!(
            "透明层 compute 三件就绪: cluster_build（单集）/ transparent_collect + gather（[set2, cluster_set] 双集 + 4B push）"
        );
        Ok(this)
    }

    fn drop_pipelines(&mut self) {
        for (pipeline, layout) in [
            (self.sort_pipeline, self.sort_layout),
            (self.build_pipeline, self.build_layout),
            (self.collect_pipeline, self.collect_layout),
            (self.gather_pipeline, self.gather_layout),
        ] {
            if pipeline != vk::Pipeline::null() {
                unsafe { self.device.destroy_pipeline(pipeline, None) };
            }
            if layout != vk::PipelineLayout::null() {
                unsafe { self.device.destroy_pipeline_layout(layout, None) };
            }
        }
        self.sort_pipeline = vk::Pipeline::null();
        self.sort_layout = vk::PipelineLayout::null();
        self.build_pipeline = vk::Pipeline::null();
        self.build_layout = vk::PipelineLayout::null();
        self.collect_pipeline = vk::Pipeline::null();
        self.collect_layout = vk::PipelineLayout::null();
        self.gather_pipeline = vk::Pipeline::null();
        self.gather_layout = vk::PipelineLayout::null();
    }

    /// cluster 构建 dispatch 输入。
    #[must_use]
    pub fn build(&self, grid: ClusterGrid) -> ClusterBuildDraw {
        ClusterBuildDraw {
            pipeline: self.build_pipeline,
            layout: self.build_layout,
            groups: grid.cluster_count().div_ceil(64),
        }
    }

    /// 透明行收集 dispatch 输入（count = 在场行数上界打包进 push）。
    ///
    /// # Errors
    /// 在场行数超出 [`TRANSPARENT_CAPACITY`]（显式报错不截断）。
    pub fn collect(&self, count: u32) -> Result<TransparentCollectDraw> {
        if count > TRANSPARENT_CAPACITY {
            return Err(VulkanError::Upload(format!(
                "在场行数 {count} 超出透明行容量 {TRANSPARENT_CAPACITY}——显式报错不截断"
            )));
        }
        let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
        push.copy_from_slice(&count.to_le_bytes());
        Ok(TransparentCollectDraw {
            pipeline: self.collect_pipeline,
            layout: self.collect_layout,
            push,
            groups: count.div_ceil(64),
        })
    }

    /// 透明命令组装 dispatch 输入（count = 在场行数上界，guard 用）。
    #[must_use]
    pub fn gather(&self, count: u32) -> TransparentGatherDraw {
        let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
        push.copy_from_slice(&count.to_le_bytes());
        TransparentGatherDraw {
            pipeline: self.gather_pipeline,
            layout: self.gather_layout,
            push,
            groups: count.div_ceil(64),
        }
    }

    /// bitonic 排序计划（8.1.4）：管线 + 组数 + 恒定 (k, j) 序列（逐帧
    /// clone——840B，免借用纠缠）。序列在建管线时预生成（容量恒 16384）。
    #[must_use]
    pub fn sort_plan(&self) -> TransparentSortPlan {
        TransparentSortPlan {
            pipeline: self.sort_pipeline,
            layout: self.sort_layout,
            groups: (TRANSPARENT_CAPACITY / 2).div_ceil(64),
            passes: self.sort_passes.clone(),
        }
    }
}

/// bitonic (k, j) 派发序列：`for k in 2..=N { for j in k/2..=1 { (k, j) } }`
/// （升序网络；N 必须为 2 的幂——容量常量由调用方保证）。
fn bitonic_sequence(n: u32) -> Vec<[u8; TRANSPARENT_SORT_PUSH as usize]> {
    assert!(n.is_power_of_two(), "bitonic 容量必须是 2 的幂");
    let mut passes = Vec::new();
    let mut k = 2u32;
    while k <= n {
        let mut j = k / 2;
        while j >= 1 {
            let mut push = [0u8; TRANSPARENT_SORT_PUSH as usize];
            push[0..4].copy_from_slice(&k.to_le_bytes());
            push[4..8].copy_from_slice(&j.to_le_bytes());
            passes.push(push);
            j /= 2;
        }
        k <<= 1;
    }
    passes
}

/// compute 管线制式件（与 cull.rs 同形状；`push_size` = 0 表示无 push range）。
fn make_compute(
    ctx: &Context,
    spv: &[u8],
    set_layouts: &[vk::DescriptorSetLayout],
    push_size: u32,
) -> Result<(vk::Pipeline, vk::PipelineLayout)> {
    let device = &ctx.device;
    let words: Vec<u32> = spv
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let module = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }?;
    let mut push_ranges = Vec::new();
    if push_size > 0 {
        push_ranges.push(
            vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .offset(0)
                .size(push_size),
        );
    }
    unsafe {
        let layout = device
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(set_layouts)
                    .push_constant_ranges(&push_ranges),
                None,
            )
            .inspect_err(|_| device.destroy_shader_module(module, None))?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"cs_main");
        let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
        match device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) {
            Ok(mut pipelines) => {
                let pipeline = pipelines.pop().expect("单 info 必返单管线");
                device.destroy_shader_module(module, None);
                Ok((pipeline, layout))
            }
            Err((pipelines, e)) => {
                for p in pipelines {
                    device.destroy_pipeline(p, None);
                }
                device.destroy_pipeline_layout(layout, None);
                device.destroy_shader_module(module, None);
                Err(e.into())
            }
        }
    }
}

impl Drop for TransparentPasses {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown 的 device_wait_idle 先行；
        // layout 借用的 set2/cluster layout 归各自 Resource 拆
        self.drop_pipelines();
    }
}

/// 录制 cluster 构建 dispatch（渲染进场前的 compute 区间）：绑管线 →
/// cluster 集 → dispatch → 表写读屏障（compute 写 → 透明 FS/后续 compute 读；
/// 诊断直读走 fence 纪律，与标记环同链）。
pub(crate) fn record_cluster_build(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    cluster_set: vk::DescriptorSet,
    draw: &ClusterBuildDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, draw.pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            draw.layout,
            0,
            std::slice::from_ref(&cluster_set),
            &[],
        );
        device.cmd_dispatch(command_buffer, draw.groups, 1, 1);
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&barrier),
            &[],
            &[],
        );
    }
}

/// 录制透明行收集 dispatch：dispatch → 列表/键/计数的 compute 写读屏障。
pub(crate) fn record_transparent_collect(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    cluster_set: vk::DescriptorSet,
    draw: &TransparentCollectDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, draw.pipeline);
        let sets = [set2, cluster_set];
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            draw.layout,
            0,
            &sets,
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            draw.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &draw.push,
        );
        device.cmd_dispatch(command_buffer, draw.groups, 1, 1);
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&barrier),
            &[],
            &[],
        );
    }
}

/// 录制 bitonic 排序链（8.1.4，collect 与 gather 之间）：逐 (k, j) 派发 +
/// 派发间 compute rw 屏障（上一段的排序写 → 下一段的排序读写）。
pub(crate) fn record_transparent_sort(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    cluster_set: vk::DescriptorSet,
    plan: &TransparentSortPlan,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, plan.pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            plan.layout,
            0,
            std::slice::from_ref(&cluster_set),
            &[],
        );
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        for pass in &plan.passes {
            device.cmd_push_constants(
                command_buffer,
                plan.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                pass,
            );
            device.cmd_dispatch(command_buffer, plan.groups, 1, 1);
            device.cmd_pipeline_barrier(
                command_buffer,
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

/// 录制透明命令组装 dispatch：dispatch → 命令环屏障（compute 写 →
/// DRAW_INDIRECT 读，与 5.3 组装同形状）。
pub(crate) fn record_transparent_gather(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    cluster_set: vk::DescriptorSet,
    draw: &TransparentGatherDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, draw.pipeline);
        let sets = [set2, cluster_set];
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            draw.layout,
            0,
            &sets,
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            draw.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &draw.push,
        );
        device.cmd_dispatch(command_buffer, draw.groups, 1, 1);
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::INDIRECT_COMMAND_READ);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&barrier),
            &[],
            &[],
        );
    }
}

// ============ 透明图形管线 ============

/// 透明前向图形管线：HDR 中间目标（alpha blending）+ 不透明深度重绑
///（GREATER 只测不写）+ 常驻三集 + cluster 小集（第 4 set）。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPipeline {
    device: Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl TransparentPipeline {
    /// 建管线链（与 GraphicsPipeline 同纪律：声明面 = 实例附件全集，
    /// VUID-08914；blend 只开在唯一 color 附件上）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        color_format: vk::Format,
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
        cluster_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = TRANSPARENT_FORWARD_SPV
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
        }?;
        let result = unsafe {
            Self::new_inner(
                device, color_format, depth_format, set0_layout, set1_layout, set2_layout,
                cluster_layout, module,
            )
        };
        if result.is_err() {
            unsafe { device.destroy_shader_module(module, None) };
        }
        result
    }

    /// # Safety
    /// `module` 须刚创建成功且未销毁。
    #[expect(
        clippy::too_many_arguments,
        reason = "Vulkan 创建链的参数表即依赖清单（内部链后半段），逐项显式"
    )]
    unsafe fn new_inner(
        device: &Device,
        color_format: vk::Format,
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
        cluster_layout: vk::DescriptorSetLayout,
        module: vk::ShaderModule,
    ) -> Result<Self> {
        unsafe {
            let set_layouts = [set0_layout, set1_layout, set2_layout, cluster_layout];
            let layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )?;
            let result = Self::create_pipeline(device, color_format, depth_format, layout, module);
            if result.is_err() {
                device.destroy_pipeline_layout(layout, None);
            }
            result
        }
    }

    /// # Safety
    /// `module`/`layout` 须为合法句柄；入口名存在于 module。
    unsafe fn create_pipeline(
        device: &Device,
        color_format: vk::Format,
        depth_format: vk::Format,
        layout: vk::PipelineLayout,
        module: vk::ShaderModule,
    ) -> Result<Self> {
        unsafe {
            let vs_main = c"vs_main";
            let fs_main = c"fs_main";
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(module)
                    .name(vs_main),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(module)
                    .name(fs_main),
            ];
            // 顶点输入 = mesh_convert 的 32B 交错契约（与场景管线同源）
            let vertex_binding = [vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(crate::render::VERTEX_STRIDE as u32)
                .input_rate(vk::VertexInputRate::VERTEX)];
            let vertex_attributes = [
                vk::VertexInputAttributeDescription::default()
                    .location(0)
                    .binding(0)
                    .format(vk::Format::R32G32B32_SFLOAT)
                    .offset(0),
                vk::VertexInputAttributeDescription::default()
                    .location(1)
                    .binding(0)
                    .format(vk::Format::R32G32B32_SFLOAT)
                    .offset(12),
                vk::VertexInputAttributeDescription::default()
                    .location(2)
                    .binding(0)
                    .format(vk::Format::R32G32_SFLOAT)
                    .offset(24),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
                .vertex_binding_descriptions(&vertex_binding)
                .vertex_attribute_descriptions(&vertex_attributes);
            let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            // 不透明深度重绑：只测（GREATER，reverse-Z 成套）不写（透明不占深度）
            let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
                .depth_test_enable(true)
                .depth_write_enable(false)
                .depth_compare_op(vk::CompareOp::GREATER)
                .min_depth_bounds(0.0)
                .max_depth_bounds(1.0);
            // 直色 alpha 混合（非预乘）：src·α + dst·(1−α)；alpha 通道直写
            let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
                .alpha_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(&blend_attachment);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic_info =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            let color_formats = [color_format];
            let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
                .color_attachment_formats(&color_formats)
                .depth_attachment_format(depth_format);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_info)
                .layout(layout)
                .push_next(&mut rendering_info);
            let pipeline = device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)?
                .remove(0);
            device.destroy_shader_module(module, None);
            info!(
                "透明前向管线就绪: HDR {color_format:?}（SRC_ALPHA 混合）+ 深度 {depth_format:?}（重绑只测不写），常驻三集 + cluster 小集"
            );
            Ok(Self {
                device: device.clone(),
                layout,
                pipeline,
            })
        }
    }

    /// 管线句柄。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }

    /// layout 句柄。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }
}

// # Safety:句柄本质是整数,创建/销毁只在持有方线程;方法只读句柄
unsafe impl Send for TransparentPipeline {}
unsafe impl Sync for TransparentPipeline {}

impl Drop for TransparentPipeline {
    fn drop(&mut self) {
        // 契约边界:不等 GPU,退出排空由 teardown 的 device_wait_idle 先行;
        // layout 借用的 set layouts 归调用方拆除
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}
