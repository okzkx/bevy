//! 透明 Clustered Forward 层：3D cluster 光源分簇 + 透明命令
//! GPU 组装 + 透明前向绘制管线。
//!
//! 数据流（全部 GPU 侧决策——GPU-driven 红线：透明行的筛选/排序/命令组装
//! 都在 compute，CPU 只发在账上界）：
//! 1. `cluster_build`（compute）：视锥按 32px tile × 32 深度片分簇（**开工即
//!    3D cluster**），逐 cluster 以世界空间 AABB（
//!    inv_view_proj 投 8 角点）与点光球求交，写光源索引表（DEVICE_LOCAL）+
//!    逐 cluster 计数（HOST_VISIBLE——`ash_renderer/cluster_census` 逐 tile
//!    对照的读数面，判定线②）。
//! 2. `transparent_collect`（compute）：从在场环筛材质 alpha<1 的行，atomicAdd
//!    紧凑成"透明行索引 + 深度键"列表——深度键是 bitonic 排序的输入，
//!    本段先按在场序画（物体级排序归 transparent_sort）。
//! 3. `transparent_gather`（compute）：索引列表 → 透明命令环（与场景命令同
//!    20B 布局，LOD 引脚选择同 build_indirect 语义）。
//! 4. 透明前向绘制：`cmd_draw_indexed_indirect`（count = 在场行数上界，非
//!    透明行 instanceCount=0），alpha blending 落 HDR 中间目标；不透明深度对
//!    透明的复用形态 = **附件重绑**（坑点：深度附件不可采样，
//!    取数通道要多一张取数目标+拷贝；附件重绑免拷贝零带宽——透明 pass 把
//!    G-buffer 深度再挂回 depth attachment，loadOp LOAD、只测不写）。
//!
//! 光源数据 = **光源常驻表**（[`crate::render::shading::LightTable`]）——本模块
//! 只持有描述符引用（b1 绑定表环），表的槽位
//! 账本/脏行重写归 light_table 模块；光源注入经 BRP `ash_renderer/light_*`
//! 方法族。
//!
//! CPU 参照（[`cluster_reference`] 纯函数 + 单测）：与 cluster_build.wgsl
//! 同公式（分片边界/AABB 投影/球-AABB 求交），供 BRP census 对照。

use ash::{vk, Device};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::bytes::{put_f32, put_mat4, put_u32};
use crate::common::error::{Result, VulkanError};
use crate::render::gpu::compute::{create_shader_module, ComputeHandles, ComputePipeline, Handoff};
use crate::render::core::resources::{BufferRole, GpuBuffer};
use crate::render::shading::LightRecord;
use crate::render::Context;
use super::pipeline_handle::create_graphics_pipeline;

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
pub(crate) const CLUSTER_TILE_SIZE: u32 = 32;
/// 深度分片数（视图深度指数划分）。
pub(crate) const CLUSTER_SLICES: u32 = 32;
/// 单 cluster 光源索引容量（超出截断——CPU 参照同语义，计数面照实对照）。
pub(crate) const MAX_PER_CLUSTER: u32 = 16;
/// 深度分片区间（视图深度，米）。
pub(crate) const CLUSTER_NEAR: f32 = 0.1;
pub(crate) const CLUSTER_FAR: f32 = 200.0;
/// 透明行列表容量（索引/键/命令三环同容量；在场行数作上界在此截断并在
/// 诊断读数显式报——与对象表容量纪律同族，学习项目规模下不预期触顶）。
pub const TRANSPARENT_CAPACITY: u32 = 16384;

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
pub(crate) fn cluster_grid(extent: vk::Extent2D) -> ClusterGrid {
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
pub(crate) fn slice_view_depth(slice: u32, grid: ClusterGrid) -> (f32, f32) {
    let ratio = CLUSTER_FAR / CLUSTER_NEAR;
    // libm powf（bevy_math::ops——仓库 disallowed-methods 纪律；WGSL 端 pow 同式）
    let z0 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, slice as f32 / grid.slices as f32);
    let z1 = CLUSTER_NEAR * bevy::math::ops::powf(ratio, (slice as f32 + 1.0) / grid.slices as f32);
    (z0, z1)
}

/// 视图深度 → NDC z（与 cluster_build.wgsl 同式）。透视投影下 clip.w = 视图
/// 深度 d，NDC z = a + b/d 恒成立（正反 Z、有限/无限远平面皆然）：反投影中心
/// 射线上两个已知 NDC 深度的点量出 d，即可解出 a、b。
/// 坑点：不能把视空间点 (0,0,−z) 直接乘 view_proj——那是把视空间当世界空间，
/// 相机不在原点时深度片整体错位，cluster 列表按 tile 阶梯状漏灯。
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
            // 死灯门卫（回收槽清零 radius=0，与 cluster_build.wgsl 同式）
            if r <= 0.0 {
                continue;
            }
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
    /// 建资源链：逐槽 buffer → set 布局（b0..b7，尺寸无关只建一次）→ pool +
    /// 描述符写入。半建失败由 Drop 回收（全有或全无）。
    ///
    /// # Errors
    /// buffer 创建/内存分配/描述符对象创建失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D, light_buffers: &[vk::Buffer]) -> Result<Self> {
        let grid = cluster_grid(extent);
        let mut this = Self {
            device: ctx.device.clone(),
            extent: vk::Extent2D::default(),
            grid,
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            uniforms: Vec::new(),
            cluster_lists: Vec::new(),
            cluster_counts: Vec::new(),
            t_index_lists: Vec::new(),
            t_keys: Vec::new(),
            t_commands: Vec::new(),
            t_counts: Vec::new(),
        };
        let slots = slot_count();
        this.build_buffers(ctx, slots)?;
        this.build_set_layout(ctx)?;
        unsafe { this.build_descriptors(ctx, slots, light_buffers) }?;
        this.extent = extent;
        info!(
            "透明层资源就绪: cluster 网格 {}×{}×{}（{} clusters，tile {CLUSTER_TILE_SIZE}px）\
             × {} 帧槽；光源 = 常驻灯表(b1 绑定)、单 cluster 上限 {MAX_PER_CLUSTER}、\
             透明行容量 {TRANSPARENT_CAPACITY}（命令环 + 索引/键列表；排序归 transparent_sort）",
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

    fn build_set_layout(&mut self, ctx: &Context) -> Result<()> {
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
        Ok(())
    }

    /// 建 pool + 逐槽分配写入描述符集（旧 pool 须已由 drop_resources 销毁）。
    ///
    /// # Safety
    /// `set_layout` 须已创建成功。
    unsafe fn build_descriptors(
        &mut self,
        ctx: &Context,
        slots: usize,
        light_buffers: &[vk::Buffer],
    ) -> Result<()> {
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
                // b1 = 光源常驻表本帧槽环（本体住 LightTable，本结构只持描述符引用）
                let light = *light_buffers.get(slot % light_buffers.len()).ok_or_else(|| {
                    VulkanError::Init("光源常驻表帧槽环数量少于 cluster 帧槽".into())
                })?;
                let groups: [vk::Buffer; 8] = [
                    self.uniforms[slot].buffer(),
                    light,
                    self.cluster_lists[slot].buffer(),
                    self.cluster_counts[slot].buffer(),
                    self.t_index_lists[slot].buffer(),
                    self.t_keys[slot].buffer(),
                    self.t_commands[slot].buffer(),
                    self.t_counts[slot].buffer(),
                ];
                for buffer in &groups {
                    infos.push(
                        vk::DescriptorBufferInfo::default()
                            .buffer(*buffer)
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

    /// 第 `slot` 帧槽的透明命令环句柄（draw 消费）。
    #[must_use]
    pub fn t_command_buffer(&self, slot: usize) -> vk::Buffer {
        self.t_commands[slot % self.t_commands.len()].buffer()
    }

    /// 第 `slot` 帧槽的深度键 buffer 句柄（帧首 -inf fill 的目标）。
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
        crate::render::core::readback::readback_buffer(
            ctx,
            self.t_index_lists[slot % self.t_index_lists.len()].buffer(),
            indices,
        )?;
        crate::render::core::readback::readback_buffer(
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

    /// 第 `slot` 帧槽的透明行索引列表回读（判定线③对照面；诊断一次性
    /// staging，调用方为低频 BRP，不在帧循环内）。
    ///
    /// # Errors
    /// 读回提交失败（设备丢失等）。
    pub fn readback_index_list(&self, ctx: &Context, slot: usize, bytes: &mut [u8]) -> Result<()> {
        crate::render::core::readback::readback_buffer(
            ctx,
            self.t_index_lists[slot % self.t_index_lists.len()].buffer(),
            bytes,
        )
    }

    /// 随 swapchain 尺寸重建（幂等：extent 没变就空手而归）。cluster 表随
    /// 网格重建（buffer + pool/描述符重建）；set 布局与尺寸无关不动。
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
        let slots = self.sets.len();
        self.drop_resources();
        self.grid = cluster_grid(extent);
        self.build_buffers(ctx, slots)?;
        unsafe { self.build_descriptors(ctx, slots, light_buffers) }?;
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
    /// bitonic 排序计划（每个非空场景帧都排序；透明行从 0 增长时也不能读未排序数组）。
    pub sort: TransparentSortPlan,
    /// set2（常驻表，gather 读；透明绘制也绑）。
    pub set2: vk::DescriptorSet,
    /// 透明前向绘制管线（layout 四集）。
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 本帧槽透明命令环（`command_count` 上界内有效）。
    pub t_commands: vk::Buffer,
    /// 本帧槽深度键 buffer（帧首 -inf fill 目标）。
    pub t_keys: vk::Buffer,
    /// 命令上界 = 在场行数（CPU 在账；非透明行 instanceCount=0）。
    pub command_count: u32,
}

/// cluster 构建 dispatch 输入（组数 = cluster 总数取整到 workgroup 64）。
#[derive(Clone, Copy)]
pub struct ClusterBuildDraw {
    pub pass: ComputeHandles,
    pub groups: u32,
}

/// 透明行收集 dispatch 输入（push = 在场行数上界）。
#[derive(Clone, Copy)]
pub struct TransparentCollectDraw {
    pub pass: ComputeHandles,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// 透明命令组装 dispatch 输入（push = 在场行数上界，guard 用）。
#[derive(Clone, Copy)]
pub struct TransparentGatherDraw {
    pub pass: ComputeHandles,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// bitonic 排序 push 步长：(k, j) 段参数 = 8B。
pub const TRANSPARENT_SORT_PUSH: u32 = 8;

/// bitonic 排序 dispatch 输入（管线 + 组数 + 预生成 (k,j) 派发序列）。
pub struct TransparentSortPlan {
    pub pass: ComputeHandles,
    /// 每派发组数 = CAPACITY/2 取整到 workgroup（容量恒定，序列恒定）。
    pub groups: u32,
    /// (k, j) 段参数序列（建管线时预生成，恒 105 项；逐帧 clone 840B——
    /// 生命周期独立于 TransparentPasses，FrameDraw 组装免借用纠缠）。
    pub passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
}

/// compute 半边四件：build / sort（[cluster_set] 单集）与 collect / gather
///（[set2, cluster_set] 双集）——与 hiz/occlusion 的双集先例同形状。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPasses {
    sort: ComputePipeline,
    /// bitonic (k, j) 派发序列（容量恒定 → 建成时一次生成）。
    sort_passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
    build: ComputePipeline,
    collect: ComputePipeline,
    gather: ComputePipeline,
}

impl TransparentPasses {
    /// 建四条 compute 管线。半建失败时已建成的 [`ComputePipeline`] 随作用域
    /// 回收（全有或全无）；layout 借用的 set2/cluster layout 归各自 Resource 拆。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        cluster_set_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &ctx.device;
        let sort_passes = bitonic_sequence(TRANSPARENT_CAPACITY)?;
        // cluster_build：单集、无 push（参数全在 uniform buffer）
        let build = ComputePipeline::new(device, CLUSTER_BUILD_SPV, &[cluster_set_layout], 0, c"cs_main")?;
        let sort = ComputePipeline::new(
            device,
            TRANSPARENT_SORT_SPV,
            &[cluster_set_layout],
            TRANSPARENT_SORT_PUSH,
            c"cs_main",
        )?;
        let pair = [set2_layout, cluster_set_layout];
        let collect =
            ComputePipeline::new(device, TRANSPARENT_COLLECT_SPV, &pair, TRANSPARENT_PUSH_SIZE, c"cs_main")?;
        let gather =
            ComputePipeline::new(device, TRANSPARENT_GATHER_SPV, &pair, TRANSPARENT_PUSH_SIZE, c"cs_main")?;
        info!(
            "透明层 compute 三件就绪: cluster_build（单集）/ transparent_collect + gather（[set2, cluster_set] 双集 + 4B push）"
        );
        Ok(Self { sort, sort_passes, build, collect, gather })
    }

    /// cluster 构建 dispatch 输入。
    #[must_use]
    pub fn build(&self, grid: ClusterGrid) -> ClusterBuildDraw {
        ClusterBuildDraw {
            pass: self.build.handles(),
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
        put_u32(&mut push, 0, count);
        Ok(TransparentCollectDraw {
            pass: self.collect.handles(),
            push,
            groups: count.div_ceil(64),
        })
    }

    /// 透明命令组装 dispatch 输入（count = 在场行数上界，guard 用）。
    #[must_use]
    pub fn gather(&self, count: u32) -> TransparentGatherDraw {
        let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
        put_u32(&mut push, 0, count);
        TransparentGatherDraw {
            pass: self.gather.handles(),
            push,
            groups: count.div_ceil(64),
        }
    }

    /// bitonic 排序计划：管线 + 组数 + 恒定 (k, j) 序列（逐帧
    /// clone——840B，免借用纠缠）。序列在建管线时预生成（容量恒 16384）。
    #[must_use]
    pub fn sort_plan(&self) -> TransparentSortPlan {
        TransparentSortPlan {
            pass: self.sort.handles(),
            groups: (TRANSPARENT_CAPACITY / 2).div_ceil(64),
            passes: self.sort_passes.clone(),
        }
    }
}

/// bitonic (k, j) 派发序列：`for k in 2..=N { for j in k/2..=1 { (k, j) } }`
/// （升序网络；N 必须为 2 的幂）。
///
/// # Errors
/// N 不是 2 的幂。
fn bitonic_sequence(n: u32) -> Result<Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>> {
    if !n.is_power_of_two() {
        return Err(VulkanError::Init(format!("bitonic 容量 {n} 不是 2 的幂")));
    }
    let mut passes = Vec::new();
    let mut k = 2u32;
    while k <= n {
        let mut j = k / 2;
        while j >= 1 {
            let mut push = [0u8; TRANSPARENT_SORT_PUSH as usize];
            put_u32(&mut push, 0, k);
            put_u32(&mut push, 4, j);
            passes.push(push);
            j /= 2;
        }
        k <<= 1;
    }
    Ok(passes)
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
    draw.pass.record(device, command_buffer, &[cluster_set], &[], draw.groups, Handoff::ComputeFragmentRead);
}

/// 录制透明行收集 dispatch：dispatch → 列表/键/计数的 compute 写读屏障。
pub(crate) fn record_transparent_collect(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    cluster_set: vk::DescriptorSet,
    draw: &TransparentCollectDraw,
) {
    draw.pass.record(device, command_buffer, &[set2, cluster_set], &draw.push, draw.groups, Handoff::ComputeRead);
}

/// 录制 bitonic 排序链（collect 与 gather 之间）：绑一次管线与集，逐 (k, j)
/// 派发 + 派发间 compute rw 屏障（上一段的排序写 → 下一段的排序读写）。
pub(crate) fn record_transparent_sort(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    cluster_set: vk::DescriptorSet,
    plan: &TransparentSortPlan,
) {
    plan.pass.bind(device, command_buffer, &[cluster_set]);
    for pass in &plan.passes {
        plan.pass.dispatch(device, command_buffer, pass, plan.groups, Handoff::ComputeReadWrite);
    }
}

/// 录制透明命令组装 dispatch：dispatch → 命令环屏障（compute 写 →
/// DRAW_INDIRECT 读，与 indirect 组装同形状）。
pub(crate) fn record_transparent_gather(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    cluster_set: vk::DescriptorSet,
    draw: &TransparentGatherDraw,
) {
    draw.pass.record(
        device,
        command_buffer,
        &[set2, cluster_set],
        &draw.push,
        draw.groups,
        Handoff::IndirectCommandRead,
    );
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
        let module = create_shader_module(device, TRANSPARENT_FORWARD_SPV)?;
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
            let pipeline = create_graphics_pipeline(device, &info)?;
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

#[cfg(test)]
mod tests {
    use super::slice_ndc_z;

    fn bitonic_sort(entries: &mut [(f32, u32)]) {
        let n = entries.len() as u32;
        let mut k = 2;
        while k <= n {
            let mut j = k / 2;
            while j > 0 {
                for gid in 0..n / 2 {
                    let i = (gid / j) * (2 * j) + (gid % j);
                    let partner = i + j;
                    let ascending = (i & k) == 0 || k == n;
                    if (ascending && entries[i as usize].0 > entries[partner as usize].0)
                        || (!ascending && entries[i as usize].0 < entries[partner as usize].0)
                    {
                        entries.swap(i as usize, partner as usize);
                    }
                }
                j /= 2;
            }
            k *= 2;
        }
    }

    #[test]
    fn bitonic_half_dispatch_sorts_entire_network_and_keeps_indices_paired() {
        let mut entries = vec![
            (3.0_f32, 3),
            (0.0, 0),
            (2.0, 2),
            (1.0, 1),
            (7.0, 7),
            (4.0, 4),
            (6.0, 6),
            (5.0, 5),
        ];
        bitonic_sort(&mut entries);
        assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(entries.iter().map(|entry| entry.1).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn bitonic_padding_moves_before_live_entries() {
        let mut entries = vec![
            (f32::NEG_INFINITY, 0),
            (f32::NEG_INFINITY, 1),
            (f32::NEG_INFINITY, 2),
            (f32::NEG_INFINITY, 3),
            (f32::NEG_INFINITY, 4),
            (f32::NEG_INFINITY, 5),
            (9.0, 90),
            (2.0, 20),
        ];
        bitonic_sort(&mut entries);
        assert_eq!(entries[6..].iter().map(|entry| entry.1).collect::<Vec<_>>(), [20, 90]);
        assert_eq!(entries[6..].iter().map(|entry| entry.0).collect::<Vec<_>>(), [2.0, 9.0]);
    }
    use bevy::math::{Mat4, Vec3};

    /// 相机不在原点时，深度片的 NDC z 必须等于"视深为 z 的真实点"的 NDC z
    ///（bevy 同款无限远 reverse-Z 透视 + 偏离原点的 look_at 视图）。
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
