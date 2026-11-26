//! 表面缓存常驻表（9.5.2）：cell 双表 + patch 四表 + ring 配置 + 计数 + 每帧 GI UBO，
//! 描述符集 `gi`（全 COMPUTE|FRAGMENT，视图在片元端读同一批表）。
//!
//! 生命周期：进程级（首次开 GI 后常驻；容量/分辨率变更才重建，重建前 device_wait_idle）。
//! 建表时一次性写满全 buffer（cell 表 INVALID、marks 全 0、patch 表全 0、ring/计数全 0），
//! 之后只由 GPU 读写——不能用 GPU 清零 dispatch 代替（零 size dispatch 与未初始化读都踩 VUID）。
//!
//! 内存口径（起步档 Res 32 / 4 级 / 池 65536）：
//! - cell 双表：2,097,152 × 4B × 2 = 16 MiB
//! - patch 表：几何 2 MiB + 辐照度两份 8 MiB + 统计 3 MiB + 反向指针 256 KiB
//! - ring 48B + 计数 64B × 2 槽 + GI UBO 288B × 2 槽
//!
//! 与计划 §0.2 的差异（施工记录里钉明）：PatchStatistics 是 48B 而不是 32B——两份 vec4
//! 之后还有 4 个 u32（state/request/update_count/heartbeat），32B 只够放两个；
//! 总表因此从 ≈28.5 MiB 变为 ≈30.5 MiB。

use std::time::Instant;

use ash::{vk, Device};
use bevy::log::info;
use bevy::math::{IVec3, Mat4, Vec3};

use crate::common::bytes::{put_f32, put_mat4, put_u32, put_vec4};
use crate::common::error::{Result, VulkanError};

use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{
    stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::passes::gi::volume::{SurfaceCacheConfig, MAX_CASCADES};
use crate::render::resident::tables::light_table::{LightRecord, LIGHT_RECORD_SIZE};
use crate::render::Context;
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// GI 集的可见阶段：compute（回收/播种写）+ 片元（覆盖图读）+ 顶点（方片按实例号读
/// 几何/统计）。一份布局一份集，扩阶段只放宽不改语义。
const GI_STAGES: vk::ShaderStageFlags = stages(&[
    vk::ShaderStageFlags::COMPUTE,
    vk::ShaderStageFlags::FRAGMENT,
    vk::ShaderStageFlags::VERTEX,
]);

/// GI 描述符集：b0 cell→patch / b1 cell 分配标记 / b2 几何 / b3 辐照度 0 / b4 辐照度 1 /
/// b5 统计 / b6 patch→cell / b7 ring 配置 / b8 计数 / b9 GI UBO /
/// b10 光源紧凑表（9.6.2 光源侧清单的灯记录，**不是** cluster 的那份环形槽表）/
/// b11 光源侧采样清单（9.6.2）/ b12 自发光三角形表（9.6.2，每帧重建）/
/// b13 bounce 分配请求表（9.6.3，estimation 写、同帧 bounce_alloc 消费）。
pub(crate) static GI_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "gi",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(1, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(2, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(3, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(4, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(5, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(6, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(7, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(8, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(9, DescKind::UniformBuffer, GI_STAGES),
        BindingDesc::new(10, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(11, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(12, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(13, DescKind::StorageBuffer, GI_STAGES),
        // 9.7.2 滤波后的 patch 辐照度 A/B（空间滤波写 A、时域滤波写 B，屏幕侧读 B）
        BindingDesc::new(14, DescKind::StorageBuffer, GI_STAGES),
        BindingDesc::new(15, DescKind::StorageBuffer, GI_STAGES),
    ],
);

/// 每帧 GI UBO 字节数（字段偏移见 [`SurfaceCacheUniformsLayout`] 的静态断言）。
///
/// 9.5 是 256B（`prev_view_proj 64 + target_pos 16 + volume 16 + 级联 128 + ring 16 +
/// frame 16`）已排满；9.6.1 追加两个 vec4（估计参数 u32 组与 f32 组）→ 288B；
/// 9.6.2 再追加一个 vec4（光源紧凑表的灯数与清单条数）→ 304B。
pub const GI_UBO_SIZE: u64 = 304;
/// patch 几何记录：位置 vec4 + 法线 vec4。
pub const PATCH_GEOMETRY_SIZE: u64 = 32;
/// patch 辐照度记录：l0 / l1x / l1y / l1z 各一个 vec4。
pub const PATCH_IRRADIANCE_SIZE: u64 = 64;
/// patch 统计记录：mean + variance + 4 个 u32。
pub const PATCH_STATISTICS_SIZE: u64 = 48;

/// 每槽全部 patch 表：几何、估计双份、空间与时域滤波各一份、统计、反向 cell 指针。
const fn patch_table_bytes(capacity: u64) -> u64 {
    capacity * (PATCH_GEOMETRY_SIZE + PATCH_IRRADIANCE_SIZE * 4 + PATCH_STATISTICS_SIZE + 4)
}

/// ring 配置的字数（flip / active / watermark / pad + 两份 4 字段配置）。
pub const RING_CONFIG_WORDS: usize = 12;
pub const RING_CONFIG_SIZE: u64 = RING_CONFIG_WORDS as u64 * 4;
/// 每帧计数器字数与字节数。
/// 光源紧凑表容量：与 `light_table::LIGHT_TABLE_CAPACITY` 同值（9.6.2 的清单按它上限）。
pub const GI_LIGHT_CAPACITY: u32 = 256;
const _: () = assert!(
    GI_LIGHT_CAPACITY == crate::render::resident::tables::light_table::LIGHT_TABLE_CAPACITY,
    "光源紧凑表容量必须与 cluster 侧光源常驻表一致"
);
/// 光源侧采样清单上限（每帧条数上限；`gi_set` 的 `light_samples` 不得超过它）。
pub const GI_LIGHT_SAMPLE_CAPACITY: u32 = 256;
/// 一条光源侧采样记录的字节数：命中点位置 / 法线 / 反照率 / 杂项（距离、1/pdf、灯号、标记）。
pub const GI_LIGHT_SAMPLE_SIZE: u64 = 64;
const _: () = assert!(
    GI_LIGHT_SAMPLE_SIZE == LIGHT_RECORD_SIZE,
    "光源紧凑表沿用 LightRecord 的 64B 步长"
);

/// 自发光三角形表上限（超限截断，`counters[14]` 仍会记到超限值，便于读数发现）。
pub const GI_EMISSIVE_CAPACITY: u32 = 32768;
/// 一条自发光三角形记录的字节数：p0(+面积) / e1 / e2 / 自发光辐亮度。
pub const GI_EMISSIVE_TRIANGLE_SIZE: u64 = 64;
/// 每帧 bounce 分配请求的上限（超限丢弃，`counters[11]` 仍记到超限值）。
pub const GI_BOUNCE_REQUEST_CAPACITY: u32 = 4096;
/// 一条 bounce 分配请求的字节数：cell / 命中点位置 / 命中点法线。
pub const GI_BOUNCE_REQUEST_SIZE: u64 = 48;

pub const GI_COUNTER_WORDS: usize = 16;
pub const GI_COUNTERS_SIZE: u64 = GI_COUNTER_WORDS as u64 * 4;

/// 固定容量的 ring、每帧槽参数/计数/光源/自发光/请求表；滤波表已归 patch 分类。
const fn misc_table_bytes() -> u64 {
    let per_frame = GI_COUNTERS_SIZE
        + GI_UBO_SIZE
        + GI_LIGHT_CAPACITY as u64 * GI_LIGHT_SAMPLE_SIZE * 2
        + GI_EMISSIVE_CAPACITY as u64 * GI_EMISSIVE_TRIANGLE_SIZE
        + GI_BOUNCE_REQUEST_CAPACITY as u64 * GI_BOUNCE_REQUEST_SIZE;
    RING_CONFIG_SIZE + per_frame * MAX_FRAMES_IN_FLIGHT as u64
}

/// ring 配置字索引。
pub const RING_FLIP: usize = 0;
pub const RING_ACTIVE: usize = 1;
pub const RING_WATERMARK: usize = 2;
/// 两份 RingConfig 的起始字（每份 start / count / scan / pad）。
pub const RING_CONFIGS: usize = 4;

/// 每帧计数器索引（宿主每帧清本槽，GPU 累加，`gi_stats` 读）。
pub const COUNTER_ALLOC: usize = 0;
pub const COUNTER_FAILED: usize = 1;
pub const COUNTER_FREED: usize = 2;
pub const COUNTER_DEFRAG_MOVED: usize = 3;
/// 覆盖视图（形态 A）本帧统计：空洞像素数 / 位置偏离超阈值像素数 / 有 patch 像素数。
pub const COUNTER_VIEW_HOLES: usize = 4;
pub const COUNTER_VIEW_DEVIATION: usize = 5;
pub const COUNTER_VIEW_COVERED: usize = 6;
/// 9.6 估计计数（8 号留空，9 号起）：发出的射线数。
pub const COUNTER_EST_RAYS: usize = 9;
/// 9.6：命中射线条数。
pub const COUNTER_EST_HITS: usize = 10;
/// 9.6：bounce 分配请求条数。
pub const COUNTER_EST_BOUNCE_REQUESTS: usize = 11;
/// 9.6：本帧参与估计的 patch 数。
pub const COUNTER_EST_PATCHES: usize = 12;
/// 9.6：本帧样本总数（含预热倍率），用于读数换算平均样本档。
pub const COUNTER_EST_SAMPLES: usize = 13;
/// 9.6.2：本帧自发光三角形条数（超过 [`GI_EMISSIVE_CAPACITY`] 时是超限值）。
pub const COUNTER_EMISSIVE_TRIANGLES: usize = 14;
/// 9.6.3：多跳自查询命中缓存（找到 patch）的次数。
pub const COUNTER_EST_BOUNCE_HITS: usize = 8;
/// 9.6.3：多跳自查询落空（该 cell 无 patch）的次数——桶错位率的读数代理。
pub const COUNTER_EST_BOUNCE_MISSES: usize = 15;

/// 每帧 GI UBO 的宿主侧数据（`draw_frame` 组装，[`pack_surface_cache_uniforms`] 打包）。
pub struct SurfaceCacheUniformsData {
    /// 上一帧 view_proj（9.6 的种子重投影用；本段只写不用）。
    pub prev_view_proj: Mat4,
    /// Volume 焦点（主相机世界位置）。
    pub target_pos: Vec3,
    pub voxel_min: f32,
    pub res: u32,
    pub cascades: u32,
    pub capacity: u32,
    /// 各级整数体素偏移（超出级联数的项保持 0）。
    pub cascade_offsets: [IVec3; MAX_CASCADES],
    /// 本帧整理的波数。
    pub defrag_waves: u32,
    /// 法线来源：0 平面 / 1 着色。
    pub normal_source: u32,
    /// 播种开关（0 = 只回收不播种，BRP A/B 用）。
    pub seed_enabled: u32,
    pub frame_index: u32,
    /// 淘汰年龄门限（帧）。
    pub evict_age: u32,
    pub bounce_enabled: u32,
    pub update_max: u32,
    /// 9.6：每 patch 每帧样本档（预算换算后的实际值，宿主写）。
    pub samples: u32,
    /// 9.6：预热倍率（`update_count == 0 && rank == 0` 的 patch 用 `samples × warmup` 条）。
    pub warmup: u32,
    /// 9.6：多跳开关（命中点查缓存读上一帧）。
    pub multi_bounce: u32,
    /// 9.6：光源侧采样清单长度（每帧条数）。
    pub light_samples: u32,
    /// 9.6：方差引导时域混合的短时滞后（0.75～0.95）。
    pub short_hysteresis: f32,
    /// 9.6：自发光三角形强度倍数。
    pub emissive_intensity: f32,
    /// 9.6.2：光源紧凑表里活灯的条数（estimation 的 `reciprocalDensity` 用它）。
    pub light_count: u32,
    /// 9.6.2：本帧光源侧采样清单的条数（0 = 不做重连接取样）。
    pub light_sample_count: u32,
    /// 9.6.5：帧稀释步长（1 = 不稀释；N > 1 时只有 `patch_hash % N == frame % N` 的 patch
    /// 本帧参与估计）。
    pub estimation_stride: u32,
    /// 9.6.5：预算换算用的活跃 patch 数（读数对账用：与 ring 水位同源）。
    pub estimation_active: u32,
}

/// [`SurfaceCacheUniformsData`] 的 WGSL uniform 布局镜像：静态断言钉偏移。
#[repr(C)]
struct SurfaceCacheUniformsLayout {
    prev_view_proj: [f32; 16],
    target_pos: [f32; 4],
    volume: [f32; 4],
    cascade_offsets: [[i32; 4]; MAX_CASCADES],
    ring: [u32; 4],
    frame: [u32; 4],
    estimation: [u32; 4],
    estimation_f: [f32; 4],
    estimation_l: [u32; 4],
}

const _: () = {
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, prev_view_proj) == 0);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, target_pos) == 64);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, volume) == 80);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, cascade_offsets) == 96);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, ring) == 224);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, frame) == 240);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, estimation) == 256);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, estimation_f) == 272);
    assert!(std::mem::offset_of!(SurfaceCacheUniformsLayout, estimation_l) == 288);
    assert!(std::mem::size_of::<SurfaceCacheUniformsLayout>() == GI_UBO_SIZE as usize);
};

/// [`SurfaceCacheUniformsData`] → 256B uniform 字节。
#[must_use]
pub fn pack_surface_cache_uniforms(d: &SurfaceCacheUniformsData) -> [u8; GI_UBO_SIZE as usize] {
    let mut out = [0u8; GI_UBO_SIZE as usize];
    put_mat4(&mut out, 0, &d.prev_view_proj);
    put_vec4(
        &mut out,
        64,
        &[d.target_pos.x, d.target_pos.y, d.target_pos.z, 0.0],
    );
    put_vec4(
        &mut out,
        80,
        &[
            d.voxel_min,
            d.res as f32,
            d.cascades as f32,
            d.capacity as f32,
        ],
    );
    for (i, offset) in d.cascade_offsets.iter().enumerate() {
        let base = 96 + i * 16;
        put_u32(&mut out, base, offset.x as u32);
        put_u32(&mut out, base + 4, offset.y as u32);
        put_u32(&mut out, base + 8, offset.z as u32);
    }
    put_u32(&mut out, 224, d.defrag_waves);
    put_u32(&mut out, 228, d.normal_source);
    put_u32(&mut out, 232, d.seed_enabled);
    put_f32(&mut out, 236, 0.0);
    put_u32(&mut out, 240, d.frame_index);
    put_u32(&mut out, 244, d.evict_age);
    put_u32(&mut out, 248, d.bounce_enabled);
    put_u32(&mut out, 252, d.update_max);
    put_u32(&mut out, 256, d.samples);
    put_u32(&mut out, 260, d.warmup);
    put_u32(&mut out, 264, d.multi_bounce);
    put_u32(&mut out, 268, d.light_samples);
    put_f32(&mut out, 272, d.short_hysteresis);
    put_f32(&mut out, 276, d.emissive_intensity);
    put_f32(&mut out, 280, 0.0);
    put_f32(&mut out, 284, 0.0);
    put_u32(&mut out, 288, d.light_count);
    put_u32(&mut out, 292, d.light_sample_count);
    put_u32(&mut out, 296, d.estimation_stride);
    put_u32(&mut out, 300, d.estimation_active);
    out
}

/// Host-side provenance of the last submitted GI pass set (not a GPU completion token).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubmittedGiFrame {
    pub slot: usize,
    pub frame_index: u32,
    pub normal_source: u32,
    pub has_estimation: bool,
    pub has_scgi: bool,
    pub extent: [u32; 2],
}

fn gi_frame_after_submission(frame: SubmittedGiFrame, has_gi: bool) -> Option<SubmittedGiFrame> {
    has_gi.then_some(frame)
}

/// 默认种子 UBO（identity 矩阵、零体积参数）：首帧写入前被读到也是确定值。
fn identity_uniforms() -> SurfaceCacheUniformsData {
    SurfaceCacheUniformsData {
        prev_view_proj: Mat4::IDENTITY,
        target_pos: Vec3::ZERO,
        voxel_min: 0.5,
        res: 32,
        cascades: 4,
        capacity: 65_536,
        cascade_offsets: [IVec3::ZERO; MAX_CASCADES],
        defrag_waves: 2,
        normal_source: 0,
        seed_enabled: 1,
        frame_index: 0,
        evict_age: 240,
        bounce_enabled: 0,
        update_max: 32,
        samples: 2,
        warmup: 4,
        multi_bounce: 1,
        light_samples: 128,
        short_hysteresis: 0.85,
        emissive_intensity: 1.0,
        light_count: 0,
        light_sample_count: 0,
        estimation_stride: 1,
        estimation_active: 0,
    }
}

/// 显存读数（BRP 出账）。
#[derive(Clone, Copy, Debug, Default)]
pub struct SurfaceCacheMemory {
    pub cell_bytes: u64,
    pub patch_bytes: u64,
    pub misc_bytes: u64,
}

impl SurfaceCacheMemory {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.cell_bytes + self.patch_bytes + self.misc_bytes
    }
}

/// GI 集布局的持有者（进程级）：布局被管线 layout 引用，必须在管线整个生命周期内存活；
/// 表重建（改 Volume 规格）只换 buffer 与描述符集，**不换布局**——否则已建管线的
/// pipeline layout 会引用被销毁的 set layout（VUID-vkCmdBindPipeline-pipeline-parameter）。
#[derive(bevy::prelude::Resource)]
pub struct GiSetLayout {
    device: Device,
    layout: vk::DescriptorSetLayout,
}

impl GiSetLayout {
    /// # Errors
    /// 布局创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        Ok(Self {
            device: ctx.device.clone(),
            layout: GI_SET_DESC.create(&ctx.device)?,
        })
    }

    #[must_use]
    pub fn raw(&self) -> vk::DescriptorSetLayout {
        self.layout
    }

    /// 登记给管线组装。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&GI_SET_DESC, self.layout)
    }
}

impl Drop for GiSetLayout {
    fn drop(&mut self) {
        // 不等 GPU：拆除序由 init 链保证（管线与表先拆）
        unsafe { self.device.destroy_descriptor_set_layout(self.layout, None) };
    }
}

/// 表面缓存常驻表与描述符集（进程级）。
#[derive(bevy::prelude::Resource)]
pub struct SurfaceCacheTables {
    device: Device,
    config: SurfaceCacheConfig,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    ubos: Vec<GpuBuffer>,
    cell_patch: GpuBuffer,
    cell_marks: GpuBuffer,
    patch_geometry: GpuBuffer,
    patch_irradiance0: GpuBuffer,
    patch_irradiance1: GpuBuffer,
    /// 9.7.2 空间滤波输出（`b14`）：每帧 estimation 之后写，供 9.7.3 时域滤波与屏幕查表读。
    filtered_irradiance0: GpuBuffer,
    /// 9.7.3 时域滤波输出（`b15`）：屏幕查表读这一份。
    filtered_irradiance1: GpuBuffer,
    patch_statistics: GpuBuffer,
    patch_cell: GpuBuffer,
    ring: GpuBuffer,
    counters: Vec<GpuBuffer>,
    /// 9.6.2 光源紧凑表（每帧槽一份，宿主写活灯记录，`b10`）。
    gi_lights: Vec<GpuBuffer>,
    /// 9.6.2 光源侧采样清单（每帧槽一份，compute 写、estimation 读，`b11`）。
    light_samples: Vec<GpuBuffer>,
    /// 9.6.2 自发光三角形表（每帧槽一份，每帧重建，`b12`）。
    emissive_triangles: Vec<GpuBuffer>,
    /// 9.6.3 bounce 分配请求表（每帧槽一份，`b13`）。
    bounce_requests: Vec<GpuBuffer>,
    /// 帧尝试失败/让路时清空；仅成功提交实际 GI 束后发布。
    submitted_gi: Option<SubmittedGiFrame>,
    prepared_frame_index: [u32; MAX_FRAMES_IN_FLIGHT],
    memory: SurfaceCacheMemory,
}

impl SurfaceCacheTables {
    /// 按规格建全部表：一次写满初值（INVALID / 0），建 UBO 与两帧槽描述符集。
    /// `layout` 由 [`GiSetLayout`] 持有（表重建不换布局）。
    ///
    /// # Errors
    /// 规格非法、buffer/UBO/描述符集创建或初值写入失败。
    pub fn new(
        ctx: &Context,
        config: SurfaceCacheConfig,
        layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let problems = config.problems();
        if !problems.is_empty() {
            return Err(VulkanError::Init(format!(
                "表面缓存规格非法：{}",
                problems.join("；")
            )));
        }
        let device = &ctx.device;
        let contract = ctx.memory_contract();
        let cell_bytes = config.cell_count() * 4;
        let capacity = u64::from(config.capacity);
        let patch_bytes = patch_table_bytes(capacity);

        let started = Instant::now();
        let mut cell_patch = GpuBuffer::create(device, contract, cell_bytes, BufferRole::Storage)?;
        let mut cell_marks = GpuBuffer::create(device, contract, cell_bytes, BufferRole::Storage)?;
        let mut patch_geometry = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_GEOMETRY_SIZE,
            BufferRole::Storage,
        )?;
        let mut patch_irradiance0 = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_IRRADIANCE_SIZE,
            BufferRole::Storage,
        )?;
        let mut patch_irradiance1 = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_IRRADIANCE_SIZE,
            BufferRole::Storage,
        )?;
        // 9.7.2/9.7.3 滤波表：与 patch 辐照度同尺寸各一份（65536 × 64B = 4 MiB ×2）
        let mut filtered_irradiance0 = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_IRRADIANCE_SIZE,
            BufferRole::Storage,
        )?;
        let mut filtered_irradiance1 = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_IRRADIANCE_SIZE,
            BufferRole::Storage,
        )?;
        let mut patch_statistics = GpuBuffer::create(
            device,
            contract,
            capacity * PATCH_STATISTICS_SIZE,
            BufferRole::Storage,
        )?;
        let mut patch_cell =
            GpuBuffer::create(device, contract, capacity * 4, BufferRole::Storage)?;
        let mut ring = GpuBuffer::create(device, contract, RING_CONFIG_SIZE, BufferRole::Storage)?;

        // 一次性写满：cell 双表 INVALID、marks / patch 表 / ring / 计数全 0
        let invalid = vec![0xFFu8; cell_bytes as usize];
        cell_patch.write(0, &invalid)?;
        cell_marks.write(0, &vec![0u8; cell_bytes as usize])?;
        patch_geometry.write(0, &vec![0u8; (capacity * PATCH_GEOMETRY_SIZE) as usize])?;
        patch_irradiance0.write(0, &vec![0u8; (capacity * PATCH_IRRADIANCE_SIZE) as usize])?;
        patch_irradiance1.write(0, &vec![0u8; (capacity * PATCH_IRRADIANCE_SIZE) as usize])?;
        filtered_irradiance0.write(0, &vec![0u8; (capacity * PATCH_IRRADIANCE_SIZE) as usize])?;
        filtered_irradiance1.write(0, &vec![0u8; (capacity * PATCH_IRRADIANCE_SIZE) as usize])?;
        patch_statistics.write(0, &vec![0u8; (capacity * PATCH_STATISTICS_SIZE) as usize])?;
        patch_cell.write(0, &vec![0xFFu8; (capacity * 4) as usize])?;
        ring.write(0, &[0u8; RING_CONFIG_SIZE as usize])?;

        let mut ubos = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        let initial = pack_surface_cache_uniforms(&identity_uniforms());
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut ubo =
                GpuBuffer::create(device, contract, GI_UBO_SIZE, BufferRole::FrameUniform)?;
            ubo.write(0, &initial)?;
            ubos.push(ubo);
        }
        let mut counters = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut buffer =
                GpuBuffer::create(device, contract, GI_COUNTERS_SIZE, BufferRole::Storage)?;
            buffer.write(0, &[0u8; GI_COUNTERS_SIZE as usize])?;
            counters.push(buffer);
        }
        // 9.6.2：光源紧凑表与光源侧采样清单（各 2 槽 × 256 条 × 64B = 16 KiB/槽）
        let mut gi_lights = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        let mut light_samples = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        let light_bytes = u64::from(GI_LIGHT_CAPACITY) * GI_LIGHT_SAMPLE_SIZE;
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut buffer = GpuBuffer::create(device, contract, light_bytes, BufferRole::Storage)?;
            buffer.write(0, &vec![0u8; light_bytes as usize])?;
            gi_lights.push(buffer);
            let mut buffer = GpuBuffer::create(device, contract, light_bytes, BufferRole::Storage)?;
            buffer.write(0, &vec![0u8; light_bytes as usize])?;
            light_samples.push(buffer);
        }
        // 9.6.2：自发光三角形表（2 槽 × 32768 条 × 64B = 4 MiB/槽）
        let emissive_bytes = u64::from(GI_EMISSIVE_CAPACITY) * GI_EMISSIVE_TRIANGLE_SIZE;
        let mut emissive_triangles = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut buffer =
                GpuBuffer::create(device, contract, emissive_bytes, BufferRole::Storage)?;
            buffer.write(0, &vec![0u8; emissive_bytes as usize])?;
            emissive_triangles.push(buffer);
        }
        // 9.6.3：bounce 分配请求表（2 槽 × 4096 条 × 48B = 192 KiB/槽）
        let request_bytes = u64::from(GI_BOUNCE_REQUEST_CAPACITY) * GI_BOUNCE_REQUEST_SIZE;
        let mut bounce_requests = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut buffer =
                GpuBuffer::create(device, contract, request_bytes, BufferRole::Storage)?;
            buffer.write(0, &vec![0u8; request_bytes as usize])?;
            bounce_requests.push(buffer);
        }
        let init_millis = started.elapsed().as_secs_f32() * 1000.0;

        let mut this = Self {
            device: ctx.device.clone(),
            config,
            layout,
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            ubos,
            cell_patch,
            cell_marks,
            patch_geometry,
            patch_irradiance0,
            patch_irradiance1,
            filtered_irradiance0,
            filtered_irradiance1,
            patch_statistics,
            patch_cell,
            ring,
            counters,
            gi_lights,
            light_samples,
            emissive_triangles,
            bounce_requests,
            submitted_gi: None,
            prepared_frame_index: [0; MAX_FRAMES_IN_FLIGHT],
            memory: SurfaceCacheMemory {
                cell_bytes: cell_bytes * 2,
                patch_bytes,
                misc_bytes: misc_table_bytes(),
            },
        };
        this.build_sets(ctx)?;
        info!(
            "表面缓存表就绪: cell 双表 {:.1} MiB ×2 + patch {:.1} MiB（池 {}）+ ring/计数（初值写 {:.1} ms）",
            cell_bytes as f64 / (1024.0 * 1024.0),
            patch_bytes as f64 / (1024.0 * 1024.0),
            this.config.capacity,
            init_millis,
        );
        Ok(this)
    }

    fn build_sets(&mut self, ctx: &Context) -> Result<()> {
        let device = &ctx.device;
        let slots = MAX_FRAMES_IN_FLIGHT as u32;
        let pool_sizes = GI_SET_DESC.pool_sizes_vec(slots);
        self.pool = unsafe {
            device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(slots)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .inspect_err(|_| device.destroy_descriptor_set_layout(self.layout, None))?
        };
        let layouts = vec![self.layout; MAX_FRAMES_IN_FLIGHT];
        self.sets = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => sets,
            Err(e) => {
                unsafe { device.destroy_descriptor_pool(self.pool, None) };
                self.pool = vk::DescriptorPool::null();
                return Err(e.into());
            }
        };
        for (frame, set) in self.sets.iter().enumerate() {
            let mut writer = DescriptorWriter::new(&GI_SET_DESC, *set);
            writer
                .buffer(0, self.cell_patch.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(1, self.cell_marks.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(2, self.patch_geometry.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(3, self.patch_irradiance0.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(4, self.patch_irradiance1.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(5, self.patch_statistics.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(6, self.patch_cell.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(7, self.ring.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(8, self.counters[frame].buffer(), 0, vk::WHOLE_SIZE)
                .buffer(9, self.ubos[frame].buffer(), 0, GI_UBO_SIZE)
                .buffer(10, self.gi_lights[frame].buffer(), 0, vk::WHOLE_SIZE)
                .buffer(11, self.light_samples[frame].buffer(), 0, vk::WHOLE_SIZE)
                .buffer(
                    12,
                    self.emissive_triangles[frame].buffer(),
                    0,
                    vk::WHOLE_SIZE,
                )
                .buffer(13, self.bounce_requests[frame].buffer(), 0, vk::WHOLE_SIZE)
                .buffer(14, self.filtered_irradiance0.buffer(), 0, vk::WHOLE_SIZE)
                .buffer(15, self.filtered_irradiance1.buffer(), 0, vk::WHOLE_SIZE);
            writer.submit(device)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn config(&self) -> SurfaceCacheConfig {
        self.config
    }

    #[must_use]
    pub fn memory(&self) -> SurfaceCacheMemory {
        self.memory
    }

    /// 成功提交的 GI 槽，不是下一复用槽；帧让路或失败后不允许读旧图。
    /// 仍须等待 GPU，标记不是完成信号。
    #[must_use]
    pub(crate) fn submitted_gi_slot(&self) -> Option<usize> {
        self.submitted_gi.map(|frame| frame.slot)
    }

    #[must_use]
    pub(crate) fn submitted_gi_frame(&self) -> Option<SubmittedGiFrame> {
        self.submitted_gi
    }

    /// 每次帧尝试开始即撤销资格：resize/让路/致命失败不得留下旧资源的有效标记。
    pub(crate) fn invalidate_frame_submission(&mut self) {
        self.submitted_gi = None;
    }

    /// 仅在 queueSubmit 成功后按实际录制束发布；normal_source 必须是本次 push 的值。
    pub(crate) fn mark_frame_submitted(
        &mut self,
        slot: usize,
        has_gi: bool,
        normal_source: u32,
        has_estimation: bool,
        has_scgi: bool,
        extent: vk::Extent2D,
    ) {
        self.submitted_gi = gi_frame_after_submission(
            SubmittedGiFrame {
                slot,
                frame_index: self.prepared_frame_index[slot],
                normal_source,
                has_estimation,
                has_scgi,
                extent: [extent.width, extent.height],
            },
            has_gi,
        );
    }

    /// 第 `frame` 帧槽的 GI 集。
    #[must_use]
    pub fn set(&self, frame: usize) -> DescSet {
        DescSet::new(&GI_SET_DESC, self.sets[frame])
    }

    /// 写第 `frame` 帧槽 GI UBO（须在该帧槽 fence 之后）。
    ///
    /// # Errors
    /// 写入失败。
    pub fn write_uniforms(&mut self, frame: usize, data: &SurfaceCacheUniformsData) -> Result<()> {
        let bytes = pack_surface_cache_uniforms(data);
        self.ubos[frame].write(0, &bytes)?;
        self.prepared_frame_index[frame] = data.frame_index;
        Ok(())
    }

    /// 宿主清本槽计数器（wait_for_slot 之后调，上一轮 GPU 已用完本槽）。
    ///
    /// # Errors
    /// 写入失败。
    pub fn clear_counters(&mut self, frame: usize) -> Result<()> {
        self.counters[frame].write(0, &[0u8; GI_COUNTERS_SIZE as usize])
    }

    /// 第 `frame` 帧槽计数缓冲（BRP 读数）。
    pub fn counters(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.counters[frame]
    }

    /// ring 配置（BRP 读数与单元测试）。
    pub fn ring(&mut self) -> &mut GpuBuffer {
        &mut self.ring
    }

    /// 第 `frame` 帧槽 GI UBO 句柄（BRP 对账）。
    #[must_use]
    pub fn ubo_handle(&self, frame: usize) -> vk::Buffer {
        self.ubos[frame].buffer()
    }

    /// 第 `frame` 帧槽 GI UBO（BRP 读回解析体积参数与偏移）。
    pub fn ubo(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.ubos[frame]
    }

    /// cell→patch 表（BRP census 读回）。
    pub fn cell_patch(&mut self) -> &mut GpuBuffer {
        &mut self.cell_patch
    }

    /// patch→cell 表。
    pub fn patch_cell(&mut self) -> &mut GpuBuffer {
        &mut self.patch_cell
    }

    /// patch 几何表。
    pub fn patch_geometry(&mut self) -> &mut GpuBuffer {
        &mut self.patch_geometry
    }

    /// patch 统计表。
    pub fn patch_statistics(&mut self) -> &mut GpuBuffer {
        &mut self.patch_statistics
    }

    /// 辐照度第 0 份（ping-pong 的 A；9.6 的估计写 `frame&1` 选中的那一份）。
    pub fn patch_irradiance0(&mut self) -> &mut GpuBuffer {
        &mut self.patch_irradiance0
    }

    /// 辐照度第 1 份（ping-pong 的 B）。
    pub fn patch_irradiance1(&mut self) -> &mut GpuBuffer {
        &mut self.patch_irradiance1
    }

    /// 9.7.2 空间滤波输出表（BRP 在 GPU 排空后读回）。
    pub fn filtered_irradiance0(&mut self) -> &mut GpuBuffer {
        &mut self.filtered_irradiance0
    }

    /// 9.7.3 时域滤波输出表（BRP 在 GPU 排空后读回）。
    pub fn filtered_irradiance1(&mut self) -> &mut GpuBuffer {
        &mut self.filtered_irradiance1
    }

    /// 9.6.2：第 `slot` 帧槽的光源紧凑表。
    pub fn gi_lights(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.gi_lights[slot]
    }

    /// 9.6.2：把活灯记录按紧凑顺序写进本帧槽（多余槽位写 0 = 死灯）。
    ///
    /// # Errors
    /// 灯数超过 [`GI_LIGHT_CAPACITY`] 或写入失败。
    pub fn write_gi_lights(&mut self, slot: usize, lights: &[LightRecord]) -> Result<()> {
        if lights.len() > GI_LIGHT_CAPACITY as usize {
            return Err(VulkanError::Init(format!(
                "光源紧凑表超出容量：{} > {}",
                lights.len(),
                GI_LIGHT_CAPACITY
            )));
        }
        let mut bytes = vec![0u8; (u64::from(GI_LIGHT_CAPACITY) * GI_LIGHT_SAMPLE_SIZE) as usize];
        for (i, record) in lights.iter().enumerate() {
            let base = i * GI_LIGHT_SAMPLE_SIZE as usize;
            bytes[base..base + LIGHT_RECORD_SIZE as usize].copy_from_slice(&record.pack());
        }
        self.gi_lights[slot].write(0, &bytes)
    }

    /// 9.6.2：第 `slot` 帧槽的光源侧采样清单（诊断读数用）。
    pub fn light_samples(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.light_samples[slot]
    }

    /// 9.6.2：第 `slot` 帧槽的自发光三角形表（诊断读数用）。
    pub fn emissive_triangles(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.emissive_triangles[slot]
    }

    /// 9.6.3：第 `slot` 帧槽的 bounce 分配请求表（诊断读数用）。
    pub fn bounce_requests(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.bounce_requests[slot]
    }
}

impl Drop for SurfaceCacheTables {
    fn drop(&mut self) {
        // 不等 GPU：重建前调用方已 device_wait_idle，退出排空由 teardown 先行。
        // 布局不归本结构（见 GiSetLayout），只拆 pool（连带释放集）。
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitted_gi_marker_tracks_actual_graphics_submission() {
        let frame = SubmittedGiFrame {
            slot: 0,
            frame_index: 12,
            normal_source: 0,
            has_estimation: true,
            has_scgi: true,
            extent: [1600, 900],
        };
        let mut submitted = gi_frame_after_submission(frame, true);
        assert_eq!(submitted, Some(frame));
        // UI changes do not alter the already submitted normal source.
        let next = SubmittedGiFrame {
            slot: 1,
            frame_index: 13,
            normal_source: 1,
            has_estimation: false,
            has_scgi: false,
            ..frame
        };
        assert_eq!(submitted.unwrap().normal_source, 0);
        submitted = gi_frame_after_submission(next, true);
        assert_eq!(submitted, Some(next));
        assert!(!submitted.unwrap().has_estimation);
        // A skipped/cancelled attempt must clear the previous snapshot too.
        submitted = None;
        assert_eq!(submitted, None);
        submitted = gi_frame_after_submission(frame, false);
        assert_eq!(submitted, None);
        submitted = gi_frame_after_submission(frame, true);
        assert_eq!(submitted, Some(frame));
    }

    #[test]
    fn patch_memory_includes_both_filter_tables() {
        assert_eq!(patch_table_bytes(0), 0);
        assert_eq!(patch_table_bytes(1), 32 + 4 * 64 + 48 + 4);
        assert_eq!(patch_table_bytes(65_536), 22_282_240);
    }

    #[test]
    fn aggregate_memory_counts_filtered_tables_once() {
        let config = SurfaceCacheConfig {
            voxel_min: 0.5,
            res: 32,
            cascades: 4,
            capacity: 65_536,
        };
        let memory = SurfaceCacheMemory {
            cell_bytes: config.cell_count() * 4 * 2,
            patch_bytes: patch_table_bytes(u64::from(config.capacity)),
            misc_bytes: misc_table_bytes(),
        };
        assert_eq!(memory.cell_bytes, 16_777_216);
        assert_eq!(memory.patch_bytes, 22_282_240);
        assert_eq!(memory.misc_bytes, 4_653_840);
        assert_eq!(memory.total(), 43_713_296);
    }

    /// UBO 打包偏移与字段值逐项对账（跨端布局的宿主侧锚点）。
    #[test]
    fn uniforms_pack_at_documented_offsets() {
        let mut data = identity_uniforms();
        data.cascade_offsets[0] = IVec3::new(-3, 4, 5);
        data.cascade_offsets[3] = IVec3::new(7, -8, 9);
        data.defrag_waves = 2;
        data.normal_source = 1;
        data.seed_enabled = 0;
        data.frame_index = 123;
        data.evict_age = 240;
        data.bounce_enabled = 0;
        data.update_max = 32;
        let bytes = pack_surface_cache_uniforms(&data);
        let word = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        assert_eq!(word(64), 0.0f32.to_bits());
        assert_eq!(word(96), (-3i32) as u32);
        assert_eq!(word(96 + 4), 4);
        assert_eq!(word(96 + 8), 5);
        assert_eq!(word(96 + 3 * 16), 7);
        assert_eq!(word(96 + 3 * 16 + 4), (-8i32) as u32);
        assert_eq!(word(80), 0.5f32.to_bits());
        assert_eq!(word(80 + 4), 32.0f32.to_bits());
        assert_eq!(word(80 + 8), 4.0f32.to_bits());
        assert_eq!(word(80 + 12), 65_536.0f32.to_bits());
        assert_eq!(word(224), 2);
        assert_eq!(word(228), 1);
        assert_eq!(word(232), 0);
        assert_eq!(word(240), 123);
        assert_eq!(word(244), 240);
        assert_eq!(word(252), 32);
    }
}
