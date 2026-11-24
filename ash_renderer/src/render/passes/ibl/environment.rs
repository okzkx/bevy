//! 环境 cubemap、SH-L2 缓冲与 DFG LUT：资源族、格式能力查询、描述符集与烘焙链录制。
//!
//! 一张图两个角色（段施工计划 §0.7）：`mip 0` = 天空锐利源（捕获输出、SH 投影与预滤波
//! 的输入），`mip 1..8` = GGX 预滤波结果（高光采样）。同图靠子资源分区，捕获写 0 级、
//! 预滤波写 1..8 级，互不同子资源；整流保持 `GENERAL`，写完一次性转
//! `SHADER_READ_ONLY`（采样侧描述符声明的布局）。
//!
//! 生命周期：进程级（不随 swapchain 重建）。首次使用在帧录制里补一次布局初始化
//! （`UNDEFINED → SHADER_READ_ONLY` + 缓冲清零），否则合成/透明的描述符绑着
//! `UNDEFINED` 布局会在第一条 draw 上报布局不匹配。
//!
//! 烘焙链（脏帧才录，`ibl_enabled=false` 整段不录）：
//! `sky_capture`（mip0）→ `sh_project`（1024 方向）→ `sh_reduce`（9 系数）→
//! `sh_check`（两条自检）→ `global_probe`（16 桶 L1）→ `prefilter`（mip1..8，逐级
//! dispatch）；`brdf_lut` 只在首次生成一次。

use std::time::Instant;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{
    merged_pool_sizes, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts,
};
use crate::render::core::binding::shader_abi;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::barrier::{ImageBarrier, MemoryBarrierDesc};
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::passes::sky::SkySettings;
use crate::render::resident::tables::frame_uniform_table::FRAME_UBO_SIZE;
use crate::render::{Context, FrameUniformTable, OneShotReadback, MAX_FRAMES_IN_FLIGHT};

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;
const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// 环境 cubemap 边长（像素）。
pub const IBL_CUBE_SIZE: u32 = 256;
/// 环境 cubemap 级数：256 → 1 的全链；`mip 0` 是锐利源，1..8 是预滤波结果。
pub const IBL_CUBE_MIPS: u32 = 9;
/// 环境与 DFG LUT 共用的格式（RGBA16F）：一张能力查询覆盖两者。
const IBL_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// 环境 cubemap 的层数（六面）。
const CUBE_LAYERS: u32 = 6;
/// 采样时节选预滤波 mip 的上界 = 级数 − 1。
pub const IBL_MAX_MIP: f32 = (IBL_CUBE_MIPS - 1) as f32;

/// DFG LUT 边长（像素；x = NoV、y = perceptualRoughness）。
const DFG_LUT_SIZE: u32 = 128;

/// SH 投影方向数（1024 起；样本太少 Y22 等高阶项会抖）。
const SH_SAMPLES: u32 = 1024;
/// SH 投影的线程数 = partial 表行数（sh_reduce 按同值归约）。
const SH_THREADS: u32 = 256;
/// 每条 partial 行的浮点数（9 系数 × RGB）。
const SH_ROW_FLOATS: usize = 27;
const PARTIAL_BUFFER_SIZE: u64 = SH_THREADS as u64 * SH_ROW_FLOATS as u64 * 4;
const RADIANCE_BUFFER_SIZE: u64 = SH_SAMPLES as u64 * 16;
/// SH 缓冲：9 行 vec4（xyz = RGB，w = 0）。
pub const IBL_SH_BUFFER_SIZE: u64 = 9 * 16;
/// 自检 stats：`[0]` = l=0 相对误差、`[1..15]` = 14 个法线的相对误差；末位留对齐。
pub const IBL_STATS_FLOATS: usize = 16;
pub const IBL_STATS_BUFFER_SIZE: u64 = IBL_STATS_FLOATS as u64 * 4;

/// GlobalProbe 桶网格边长：4×4 = 16 桶（与 shader 的 `BUCKET_RESOLUTION` 同值）。
pub const IBL_PROBE_RESOLUTION: u32 = 4;
/// GlobalProbe 桶数（= resolution²）。
pub const IBL_PROBE_BUCKETS: u32 = IBL_PROBE_RESOLUTION * IBL_PROBE_RESOLUTION;
/// 桶缓冲：每桶 4 个 vec4（l0 / l1x / l1y / l1z，w 未用）= 16×64B = 1KB。
pub const IBL_PROBE_BUFFER_SIZE: u64 = IBL_PROBE_BUCKETS as u64 * 4 * 16;

/// 两次捕获的最小间隔（秒）：TOD 连续变化时 IBL 允许滞后这个窗口。
const MIN_CAPTURE_INTERVAL_SECS: f32 = 0.1;
/// 捕获 hash 里相机海拔的量化步长（米）：地面漫游永不触发重烘，飞行按需。
const ALTITUDE_HASH_STEP: f32 = 100.0;

/// 预滤波逐级采样数（mip 1..8）：锐利级样本多、粗糙级 lobe 接近半球，少也平滑。
const PREFILTER_SAMPLES: [u32; (IBL_CUBE_MIPS - 1) as usize] = [128, 96, 64, 48, 32, 24, 16, 8];
/// 预滤波 push 字节数（`PushPrefilter`：p f32 + sample_count u32）。
const PREFILTER_PUSH_SIZE: u32 = 8;

// ===== 描述符集布局 =====

/// 捕获：b0 = mip0 的 2D-array storage 视图（6 层），b1 = 帧 UBO（天空参数/海拔）。
pub(crate) static IBL_CAPTURE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_capture",
    &[
        BindingDesc::new(0, DescKind::StorageImage, CS),
        BindingDesc::new(1, DescKind::UniformBuffer, CS),
    ],
);

/// SH 投影：b0 = mip0 cube 采样、b1 = 采样器、b2 = radiance 缓存、b3 = partial 表。
pub(crate) static IBL_SH_PROJECT_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_sh_project",
    &[
        BindingDesc::new(0, DescKind::SampledImage, CS),
        BindingDesc::new(1, DescKind::Sampler, CS),
        BindingDesc::new(2, DescKind::StorageBuffer, CS),
        BindingDesc::new(3, DescKind::StorageBuffer, CS),
    ],
);

/// SH 归约：b0 = partial 表、b1 = SH 缓冲。
pub(crate) static IBL_SH_REDUCE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_sh_reduce",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, CS),
        BindingDesc::new(1, DescKind::StorageBuffer, CS),
    ],
);

/// SH 自检：b0 = radiance 缓存、b1 = SH 缓冲、b2 = stats。
pub(crate) static IBL_SH_CHECK_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_sh_check",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, CS),
        BindingDesc::new(1, DescKind::StorageBuffer, CS),
        BindingDesc::new(2, DescKind::StorageBuffer, CS),
    ],
);

/// GlobalProbe 投影：b0 = mip0 cube 采样、b1 = 采样器、b2 = 16 桶 buffer。
pub(crate) static IBL_GLOBAL_PROBE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_global_probe",
    &[
        BindingDesc::new(0, DescKind::SampledImage, CS),
        BindingDesc::new(1, DescKind::Sampler, CS),
        BindingDesc::new(2, DescKind::StorageBuffer, CS),
    ],
);

/// 预滤波：b0 = mip0 cube 采样、b1 = 采样器、b2 = 目标 mip 的 2D-array storage 视图。
pub(crate) static IBL_PREFILTER_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_prefilter",
    &[
        BindingDesc::new(0, DescKind::SampledImage, CS),
        BindingDesc::new(1, DescKind::Sampler, CS),
        BindingDesc::new(2, DescKind::StorageImage, CS),
    ],
);

/// DFG LUT 生成：b0 = 目标 2D storage。
pub(crate) static IBL_DFG_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl_dfg",
    &[BindingDesc::new(0, DescKind::StorageImage, CS)],
);

/// 合成/透明前向读的 IBL 集（两条管线的 group 4）：b0 = 预滤波 cube（全 mip）、
/// b1 = 采样器（cube 与 LUT 共用）、b2 = DFG LUT、b3 = SH 系数（9×vec4）、
/// b4 = GlobalProbe 16 桶（16×4 vec4 = 1KB，§9.3）。
pub(crate) static IBL_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "ibl",
    &[
        BindingDesc::new(0, DescKind::SampledImage, FS),
        BindingDesc::new(1, DescKind::Sampler, FS),
        BindingDesc::new(2, DescKind::SampledImage, FS),
        BindingDesc::new(3, DescKind::StorageBuffer, FS),
        BindingDesc::new(4, DescKind::StorageBuffer, FS),
    ],
);

// ===== 管线配方 =====

pub(crate) static SKY_CAPTURE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_sky_capture",
    shader: &shader_abi::SKY_CAPTURE,
    entries: &[c"cs_main"],
    sets: &[&IBL_CAPTURE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

pub(crate) static SH_PROJECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_sh_project",
    shader: &shader_abi::SH_PROJECT,
    entries: &[c"cs_main"],
    sets: &[&IBL_SH_PROJECT_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

pub(crate) static SH_REDUCE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_sh_reduce",
    shader: &shader_abi::SH_REDUCE,
    entries: &[c"cs_main"],
    sets: &[&IBL_SH_REDUCE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

pub(crate) static SH_CHECK_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_sh_check",
    shader: &shader_abi::SH_CHECK,
    entries: &[c"cs_main"],
    sets: &[&IBL_SH_CHECK_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

pub(crate) static GLOBAL_PROBE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_global_probe",
    shader: &shader_abi::GLOBAL_PROBE,
    entries: &[c"cs_main"],
    sets: &[&IBL_GLOBAL_PROBE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

pub(crate) static PREFILTER_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_prefilter",
    shader: &shader_abi::PREFILTER,
    entries: &[c"cs_main"],
    sets: &[&IBL_PREFILTER_SET_DESC],
    push: Some(PushRange {
        stages: CS,
        size: PREFILTER_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

pub(crate) static BRDF_LUT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ibl_brdf_lut",
    shader: &shader_abi::BRDF_LUT,
    entries: &[c"cs_main"],
    sets: &[&IBL_DFG_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

// ===== 资源本体 =====

/// 一张 image + 它的 dedicated memory（与 Hi-Z 金字塔同款：恰量分配、偏移 0 绑定）。
struct IblImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for IblImage {
    fn drop(&mut self) {
        // 不等 GPU：退出排空在 teardown_vulkan（先于本结构 Drop）
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 环境 cubemap 的三类视图：全链 cube（采样）、mip0 cube（捕获链内部的源）、
/// 每 mip 一张 2D-array（storage 写入；WGSL storage texture 不带 mip 参数）。
struct CubeViews {
    device: Device,
    full: vk::ImageView,
    mip0: vk::ImageView,
    storage: Vec<vk::ImageView>,
}

impl Drop for CubeViews {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image_view(self.full, None);
            self.device.destroy_image_view(self.mip0, None);
            for view in self.storage.drain(..) {
                self.device.destroy_image_view(view, None);
            }
        }
    }
}

/// DFG LUT：一张 2D 图 + 一个视图（同一句柄既作 storage 目标又作采样源）。
/// 字段序 = 拆卸序的反序：`view` 先于 `image` 拆（Drop 体拆视图，字段再拆图）。
struct DfgLut {
    device: Device,
    view: vk::ImageView,
    image: IblImage,
}

impl Drop for DfgLut {
    fn drop(&mut self) {
        unsafe { self.device.destroy_image_view(self.view, None) };
    }
}

/// 烘焙计划（`draw_frame` 判定，`record_frame` 在 ② 段消费）。
#[derive(Clone, Copy, Debug, Default)]
pub struct IblBakePlan {
    /// 首次使用：cubemap/DFG LUT 布局初始化 + SH/stats 缓冲清零（一辈子一次）。
    pub init: bool,
    /// 重烘：捕获 mip0 → SH 投影/归约/自检 → 预滤波 mip1..8。
    pub capture: bool,
    /// 生成 DFG LUT（只做一次；与首次 capture 同帧）。
    pub dfg: bool,
}

impl IblBakePlan {
    /// 本帧是否要录任何东西（false = 整段跳过，零 barrier）。
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.init && !self.capture && !self.dfg
    }
}

/// 烘焙判定的输入快照（`draw_frame` 从 SkySettings / 灯光 / 相机海拔取，
/// 免得本模块反向依赖 driver 层的 `FrameLight`）。
pub struct IblBakeInput<'a> {
    pub sky: &'a SkySettings,
    pub dir_to_light: [f32; 4],
    pub light_color: [f32; 4],
    /// 相机世界海拔（物理天空以 `camera_pos.y` 当海拔；按 100 m 量化入 hash）。
    pub altitude: f32,
    /// `ibl_enabled`：关 = 不捕获（但布局初始化仍会补一次）。
    pub enabled: bool,
}

/// 烘焙账本：全部字段只进诊断与节流，不驱动画面。
#[derive(Default)]
struct BakeState {
    /// 布局初始化是否已录进命令缓冲。
    initialized: bool,
    /// 最近一次已录制捕获的 hash（None = 从未捕获；与当前 hash 不等即脏）。
    captured_hash: Option<u64>,
    /// 最近一次捕获的墙钟时刻（0.1 s 节流）。
    last_capture: Option<Instant>,
    /// DFG LUT 是否已生成（只生成一次）。
    dfg_baked: bool,
    /// 捕获计数与最近 hash（`ibl_stats` 回显）。
    capture_count: u64,
    last_hash: u64,
}

/// 环境光照的 GPU 资源族：cubemap + DFG LUT + SH/stats/中间缓冲 + 六条 compute 管线
/// + 帧描述符集（合成/透明前向的 group 4）。
///
/// 字段序即拆卸序（`Drop` 体手工拆"必须早于布局/图死"的部分，其余随字段）：
/// 视图必须早于图、管线必须早于它们引用的 set 布局。
#[derive(bevy::prelude::Resource)]
pub struct IblEnvironment {
    device: Device,
    /// 三类视图只在建期写进描述符集；运行期不再直接读（持有即保活）。
    #[expect(
        dead_code,
        reason = "持有即保活：视图在建期写进描述符集，运行期经集间接使用"
    )]
    views: CubeViews,
    cube: IblImage,
    dfg: DfgLut,
    sampler: vk::Sampler,
    sh: GpuBuffer,
    stats: GpuBuffer,
    /// GlobalProbe 16 桶（1KB；投影写、合成/透明与 BRP 读）。
    probe: GpuBuffer,
    /// 捕获链的中间缓冲（只被 SH 投影/归约/自检读写；持有即保活）。
    #[expect(
        dead_code,
        reason = "持有即保活：中间缓冲在建期写进描述符集，运行期经集间接使用"
    )]
    partial: GpuBuffer,
    #[expect(
        dead_code,
        reason = "持有即保活：中间缓冲在建期写进描述符集，运行期经集间接使用"
    )]
    radiance: GpuBuffer,
    // 六条内部管线：Drop 体里先 take 掉（它们的 pipeline layout 引用下面这些 set 布局）
    capture: Option<ComputePipeline>,
    sh_project: Option<ComputePipeline>,
    sh_reduce: Option<ComputePipeline>,
    sh_check: Option<ComputePipeline>,
    global_probe: Option<ComputePipeline>,
    prefilter: Option<ComputePipeline>,
    brdf_lut: Option<ComputePipeline>,
    /// 各内部 set 的布局（管线的素材，也是 Drop 时要销毁的句柄）。
    capture_layout: vk::DescriptorSetLayout,
    project_layout: vk::DescriptorSetLayout,
    reduce_layout: vk::DescriptorSetLayout,
    check_layout: vk::DescriptorSetLayout,
    probe_layout: vk::DescriptorSetLayout,
    prefilter_layout: vk::DescriptorSetLayout,
    dfg_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    capture_sets: Vec<vk::DescriptorSet>,
    project_set: vk::DescriptorSet,
    reduce_set: vk::DescriptorSet,
    check_set: vk::DescriptorSet,
    probe_set: vk::DescriptorSet,
    prefilter_sets: Vec<vk::DescriptorSet>,
    dfg_set: vk::DescriptorSet,
    frame_layout: vk::DescriptorSetLayout,
    frame_pool: vk::DescriptorPool,
    frame_set: vk::DescriptorSet,
    state: BakeState,
}

impl IblEnvironment {
    /// 建环境 cubemap、DFG LUT、SH/stats/probe 缓冲、七条 compute 管线与全部描述符集。
    ///
    /// # Errors
    /// 格式能力不足（缺 `STORAGE_IMAGE`/`SAMPLED_IMAGE`/线性过滤）、设备装不下 5 个
    /// set，或任一 Vulkan 创建/分配/绑定失败。
    pub fn new(ctx: &Context, frame_uniform_table: &FrameUniformTable) -> Result<Self> {
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, IBL_FORMAT)
        }
        .optimal_tiling_features;
        let required = vk::FormatFeatureFlags::STORAGE_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        if !features.contains(required) {
            return Err(VulkanError::Init(format!(
                "IBL 格式 {IBL_FORMAT:?} 的 optimal tiling features {features:?} 不含所需 {required:?}，\
                 环境 cubemap / DFG LUT 无法按该使用角色建立（不静默降级）"
            )));
        }
        info!(
            "IBL 格式能力实测: {IBL_FORMAT:?} optimal features 含 STORAGE_IMAGE | SAMPLED_IMAGE | \
             SAMPLED_IMAGE_FILTER_LINEAR；环境 cube {IBL_CUBE_SIZE}²×{CUBE_LAYERS} 面 ×{IBL_CUBE_MIPS} 级，\
             DFG LUT {DFG_LUT_SIZE}²"
        );
        // 合成/透明前向的管线布局要 5 组 set（IBL 集在 group 4），设备下限 4 不够
        let max_sets = ctx.limits().max_bound_descriptor_sets;
        if max_sets < 5 {
            return Err(VulkanError::Init(format!(
                "设备 maxBoundDescriptorSets = {max_sets} < 5：合成/透明前向的管线布局需要 \
                 set0..set4，装不下 IBL 集"
            )));
        }
        let device = ctx.device.clone();

        // ---- 图与视图（视图先建、字段先声明：拆时视图必须早于图）----
        let cube = create_image(
            ctx,
            vk::Extent3D {
                width: IBL_CUBE_SIZE,
                height: IBL_CUBE_SIZE,
                depth: 1,
            },
            IBL_CUBE_MIPS,
            CUBE_LAYERS,
            vk::ImageUsageFlags::STORAGE
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageCreateFlags::CUBE_COMPATIBLE,
            "环境 cubemap",
        )?;
        let views = build_cube_views(&device, cube.image)?;

        let dfg_image = create_image(
            ctx,
            vk::Extent3D {
                width: DFG_LUT_SIZE,
                height: DFG_LUT_SIZE,
                depth: 1,
            },
            1,
            1,
            // TRANSFER_SRC：brp ibl_census 的 LUT 读回要走图像拷贝
            vk::ImageUsageFlags::STORAGE
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageCreateFlags::empty(),
            "DFG LUT",
        )?;
        let dfg_view = unsafe {
            create_view(
                &device,
                dfg_image.image,
                vk::ImageViewType::TYPE_2D,
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )
        }?;
        let dfg = DfgLut {
            device: device.clone(),
            view: dfg_view,
            image: dfg_image,
        };

        // ---- 采样器：线性三线性 + clamp（cube 与 LUT 共用）----
        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }?;

        // ---- 缓冲：SH 与 stats 给 BRP 读数拷贝（DeviceStorage 仍带 TRANSFER_SRC）----
        let contract = ctx.memory_contract();
        let sh = GpuBuffer::create(
            &device,
            contract,
            IBL_SH_BUFFER_SIZE,
            BufferRole::DeviceStorage,
        )?;
        let stats = GpuBuffer::create(
            &device,
            contract,
            IBL_STATS_BUFFER_SIZE,
            BufferRole::DeviceStorage,
        )?;
        // GlobalProbe 16 桶：投影写、帧集/BRP 读（DeviceStorage 带 TRANSFER_SRC）
        let probe = GpuBuffer::create(
            &device,
            contract,
            IBL_PROBE_BUFFER_SIZE,
            BufferRole::DeviceStorage,
        )?;
        let partial = GpuBuffer::create(
            &device,
            contract,
            PARTIAL_BUFFER_SIZE,
            BufferRole::DeviceStorage,
        )?;
        let radiance = GpuBuffer::create(
            &device,
            contract,
            RADIANCE_BUFFER_SIZE,
            BufferRole::DeviceStorage,
        )?;

        // ---- 布局 + 单池 + 六条管线 ----
        let capture_layout = IBL_CAPTURE_SET_DESC.create(&device)?;
        let project_layout = IBL_SH_PROJECT_SET_DESC.create(&device)?;
        let reduce_layout = IBL_SH_REDUCE_SET_DESC.create(&device)?;
        let check_layout = IBL_SH_CHECK_SET_DESC.create(&device)?;
        let probe_layout = IBL_GLOBAL_PROBE_SET_DESC.create(&device)?;
        let prefilter_layout = IBL_PREFILTER_SET_DESC.create(&device)?;
        let dfg_layout = IBL_DFG_SET_DESC.create(&device)?;
        let internal = SetLayouts::new([
            SetLayout::new(&IBL_CAPTURE_SET_DESC, capture_layout),
            SetLayout::new(&IBL_SH_PROJECT_SET_DESC, project_layout),
            SetLayout::new(&IBL_SH_REDUCE_SET_DESC, reduce_layout),
            SetLayout::new(&IBL_SH_CHECK_SET_DESC, check_layout),
            SetLayout::new(&IBL_GLOBAL_PROBE_SET_DESC, probe_layout),
            SetLayout::new(&IBL_PREFILTER_SET_DESC, prefilter_layout),
            SetLayout::new(&IBL_DFG_SET_DESC, dfg_layout),
        ])?;
        let capture = ComputePipeline::new(ctx, &SKY_CAPTURE_PIPELINE, &internal)?;
        let sh_project = ComputePipeline::new(ctx, &SH_PROJECT_PIPELINE, &internal)?;
        let sh_reduce = ComputePipeline::new(ctx, &SH_REDUCE_PIPELINE, &internal)?;
        let sh_check = ComputePipeline::new(ctx, &SH_CHECK_PIPELINE, &internal)?;
        let global_probe = ComputePipeline::new(ctx, &GLOBAL_PROBE_PIPELINE, &internal)?;
        let prefilter = ComputePipeline::new(ctx, &PREFILTER_PIPELINE, &internal)?;
        let brdf_lut = ComputePipeline::new(ctx, &BRDF_LUT_PIPELINE, &internal)?;

        // 单池覆盖全部内部 set（分配即按布局全 binding 扣，见 SetLayoutDesc::add_pool_sizes）
        let prefilter_count = IBL_CUBE_MIPS - 1;
        let pool_sizes = merged_pool_sizes(&[
            (&IBL_CAPTURE_SET_DESC, MAX_FRAMES_IN_FLIGHT as u32),
            (&IBL_SH_PROJECT_SET_DESC, 1),
            (&IBL_SH_REDUCE_SET_DESC, 1),
            (&IBL_SH_CHECK_SET_DESC, 1),
            (&IBL_GLOBAL_PROBE_SET_DESC, 1),
            (&IBL_PREFILTER_SET_DESC, prefilter_count),
            (&IBL_DFG_SET_DESC, 1),
        ]);
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32 + 5 + prefilter_count)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let allocate =
            |layout: vk::DescriptorSetLayout, count: usize| -> Result<Vec<vk::DescriptorSet>> {
                let layouts = vec![layout; count];
                unsafe {
                    device
                        .allocate_descriptor_sets(
                            &vk::DescriptorSetAllocateInfo::default()
                                .descriptor_pool(pool)
                                .set_layouts(&layouts),
                        )
                        .map_err(VulkanError::from)
                }
            };
        let capture_sets = allocate(capture_layout, MAX_FRAMES_IN_FLIGHT)?;
        let project_set = allocate(project_layout, 1)?[0];
        let reduce_set = allocate(reduce_layout, 1)?[0];
        let check_set = allocate(check_layout, 1)?[0];
        let probe_set = allocate(probe_layout, 1)?[0];
        let prefilter_sets = allocate(prefilter_layout, prefilter_count as usize)?;
        let dfg_set = allocate(dfg_layout, 1)?[0];

        // ---- 写内部 set ----
        // 捕获 set 按帧槽各一份：b1 绑该槽的帧 UBO（天空参数随帧槽轮转）
        for (slot, set) in capture_sets.iter().enumerate() {
            let mut writer = DescriptorWriter::new(&IBL_CAPTURE_SET_DESC, *set);
            writer
                .image(0, views.storage[0], vk::ImageLayout::GENERAL)
                .buffer(1, frame_uniform_table.ubo_handle(slot), 0, FRAME_UBO_SIZE);
            writer.submit(&device)?;
        }
        {
            // 捕获链全程 GENERAL（同图不同子资源：写 mip0、读 mip0、写 mip1..8）
            let mut writer = DescriptorWriter::new(&IBL_SH_PROJECT_SET_DESC, project_set);
            writer
                .image(0, views.mip0, vk::ImageLayout::GENERAL)
                .sampler(1, sampler)
                .buffer(2, radiance.buffer(), 0, RADIANCE_BUFFER_SIZE)
                .buffer(3, partial.buffer(), 0, PARTIAL_BUFFER_SIZE);
            writer.submit(&device)?;
        }
        {
            let mut writer = DescriptorWriter::new(&IBL_SH_REDUCE_SET_DESC, reduce_set);
            writer
                .buffer(0, partial.buffer(), 0, PARTIAL_BUFFER_SIZE)
                .buffer(1, sh.buffer(), 0, IBL_SH_BUFFER_SIZE);
            writer.submit(&device)?;
        }
        {
            let mut writer = DescriptorWriter::new(&IBL_SH_CHECK_SET_DESC, check_set);
            writer
                .buffer(0, radiance.buffer(), 0, RADIANCE_BUFFER_SIZE)
                .buffer(1, sh.buffer(), 0, IBL_SH_BUFFER_SIZE)
                .buffer(2, stats.buffer(), 0, IBL_STATS_BUFFER_SIZE);
            writer.submit(&device)?;
        }
        {
            // GlobalProbe 投影：只读 mip0（GENERAL，与 SH 投影同源），写 16 桶 buffer
            let mut writer = DescriptorWriter::new(&IBL_GLOBAL_PROBE_SET_DESC, probe_set);
            writer
                .image(0, views.mip0, vk::ImageLayout::GENERAL)
                .sampler(1, sampler)
                .buffer(2, probe.buffer(), 0, IBL_PROBE_BUFFER_SIZE);
            writer.submit(&device)?;
        }
        for (index, set) in prefilter_sets.iter().enumerate() {
            let mut writer = DescriptorWriter::new(&IBL_PREFILTER_SET_DESC, *set);
            writer
                .image(0, views.mip0, vk::ImageLayout::GENERAL)
                .sampler(1, sampler)
                // storage 视图按目标级：mip 1..8
                .image(2, views.storage[index + 1], vk::ImageLayout::GENERAL);
            writer.submit(&device)?;
        }
        {
            let mut writer = DescriptorWriter::new(&IBL_DFG_SET_DESC, dfg_set);
            writer.image(0, dfg.view, vk::ImageLayout::GENERAL);
            writer.submit(&device)?;
        }

        // ---- 帧集（合成/透明前向 group 4）：全链 cube + 采样器 + LUT + SH ----
        let frame_layout = IBL_SET_DESC.create(&device)?;
        let frame_pool = {
            let sizes = IBL_SET_DESC.pool_sizes_vec(1);
            unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
            }?
        };
        let frame_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(frame_pool)
                    .set_layouts(std::slice::from_ref(&frame_layout)),
            )
        }
        .map_err(VulkanError::from)?[0];
        {
            let mut writer = DescriptorWriter::new(&IBL_SET_DESC, frame_set);
            writer
                .image(0, views.full, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .sampler(1, sampler)
                .image(2, dfg.view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .buffer(3, sh.buffer(), 0, IBL_SH_BUFFER_SIZE)
                .buffer(4, probe.buffer(), 0, IBL_PROBE_BUFFER_SIZE);
            writer.submit(&device)?;
        }

        info!(
            "环境光照资源就绪: cube {IBL_CUBE_SIZE}²×{CUBE_LAYERS} 面 ×{IBL_CUBE_MIPS} 级（mip0 锐利源，\
             1..8 预滤波）、DFG LUT {DFG_LUT_SIZE}²、SH {IBL_SH_BUFFER_SIZE}B、GlobalProbe \
             {IBL_PROBE_BUCKETS} 桶 {IBL_PROBE_BUFFER_SIZE}B；七条 compute 管线 \
             （capture/project/reduce/check/global_probe/prefilter/brdf_lut）；捕获节流 \
             {MIN_CAPTURE_INTERVAL_SECS}s；设备 maxBoundDescriptorSets = {max_sets}（合成/透明前向用到 5 组）"
        );
        Ok(Self {
            device,
            views,
            cube,
            dfg,
            sampler,
            sh,
            stats,
            probe,
            partial,
            radiance,
            capture: Some(capture),
            sh_project: Some(sh_project),
            sh_reduce: Some(sh_reduce),
            sh_check: Some(sh_check),
            global_probe: Some(global_probe),
            prefilter: Some(prefilter),
            brdf_lut: Some(brdf_lut),
            capture_layout,
            project_layout,
            reduce_layout,
            check_layout,
            probe_layout,
            prefilter_layout,
            dfg_layout,
            pool,
            capture_sets,
            project_set,
            reduce_set,
            check_set,
            probe_set,
            prefilter_sets,
            dfg_set,
            frame_layout,
            frame_pool,
            frame_set,
            state: BakeState::default(),
        })
    }

    /// 帧集声明的布局（登记给合成/透明前向的管线配方）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&IBL_SET_DESC, self.frame_layout)
    }

    /// 合成/透明前向绑定的 IBL 集（group 4）。
    #[must_use]
    pub fn frame_set(&self) -> DescSet {
        DescSet::new(&IBL_SET_DESC, self.frame_set)
    }

    /// SH 系数缓冲的裸句柄（BRP 读数拷贝源）。
    #[must_use]
    pub fn sh_buffer(&self) -> vk::Buffer {
        self.sh.buffer()
    }

    /// 自检 stats 缓冲的裸句柄（BRP 读数拷贝源）。
    #[must_use]
    pub fn stats_buffer(&self) -> vk::Buffer {
        self.stats.buffer()
    }

    /// GlobalProbe 16 桶缓冲的裸句柄（BRP 读数拷贝源）。
    #[must_use]
    pub fn probe_buffer(&self) -> vk::Buffer {
        self.probe.buffer()
    }

    /// 捕获计数与最近一次的 hash（`ibl_stats` 回显）。
    #[must_use]
    pub fn bake_state(&self) -> (u64, u64) {
        (self.state.capture_count, self.state.last_hash)
    }

    /// draw_frame 每帧调用：算 `sky_hash`、判脏与节流、更新捕获账本，返回本帧烘焙计划。
    /// 返回空计划 = ② 段整段跳过、零屏障。
    pub fn prepare(&mut self, input: &IblBakeInput<'_>) -> IblBakePlan {
        // 布局初始化：无论 ibl 开关都补一次（描述符绑的是 SHADER_READ_ONLY，实际布局
        // 必须先从 UNDEFINED 迁过去，否则第一条 draw 就报布局不匹配）
        let init = !self.state.initialized;
        if init {
            self.state.initialized = true;
        }
        let mut plan = IblBakePlan {
            init,
            capture: false,
            dfg: false,
        };
        if !input.enabled {
            return plan;
        }
        let hash = sky_hash(input);
        let stale = self.state.captured_hash != Some(hash);
        let dfg_missing = !self.state.dfg_baked;
        let throttled = self
            .state
            .last_capture
            .is_some_and(|t| t.elapsed().as_secs_f32() < MIN_CAPTURE_INTERVAL_SECS);
        if (stale || dfg_missing) && !throttled {
            plan.capture = true;
            plan.dfg = dfg_missing;
            self.state.captured_hash = Some(hash);
            self.state.last_capture = Some(Instant::now());
            self.state.dfg_baked = true;
            self.state.capture_count += 1;
            self.state.last_hash = hash;
        }
        plan
    }

    /// ② 段：录本帧烘焙计划（空计划直接返回）。
    ///
    /// 布局纪律（同图不同子资源）：初始化和 DFG LUT 各自一次性迁移；捕获链全程
    /// `GENERAL`（写 mip0 / 读 mip0 / 写 mip1..8），链尾一条**全子资源**屏障转
    /// `SHADER_READ_ONLY`。`ibl_enabled=false` 的非初始化帧只留一次 `UNDEFINED` 迁移。
    pub fn record_bake(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        slot: usize,
        plan: IblBakePlan,
    ) {
        if plan.is_empty() {
            return;
        }
        let color = vk::ImageAspectFlags::COLOR;
        // 读侧阶段：合成与透明前向都是片元采样；预滤波/SH 投影是 compute 采样
        let read_stage =
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::FRAGMENT_SHADER;
        unsafe {
            // ---- 首次：布局初始化 + 缓冲清零（一辈子一次）----
            if plan.init {
                ImageBarrier::transition(
                    self.cube.image,
                    color,
                    (
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    ),
                    (vk::PipelineStageFlags::TOP_OF_PIPE, read_stage),
                    (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
                )
                .with_range(0, IBL_CUBE_MIPS, 0, CUBE_LAYERS)
                .record(device, cb);
                ImageBarrier::transition(
                    self.dfg.image.image,
                    color,
                    (
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    ),
                    (vk::PipelineStageFlags::TOP_OF_PIPE, read_stage),
                    (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
                )
                .record(device, cb);
                // SH / stats / GlobalProbe 清零：首帧捕获之前被读到也是确定值
                //（0 = 无环境光/未捕获）
                device.cmd_fill_buffer(cb, self.sh.buffer(), 0, vk::WHOLE_SIZE, 0);
                device.cmd_fill_buffer(cb, self.stats.buffer(), 0, vk::WHOLE_SIZE, 0);
                device.cmd_fill_buffer(cb, self.probe.buffer(), 0, vk::WHOLE_SIZE, 0);
                MemoryBarrierDesc {
                    src_stage: vk::PipelineStageFlags::TRANSFER,
                    src_access: vk::AccessFlags::TRANSFER_WRITE,
                    dst_stage: read_stage,
                    dst_access: vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                }
                .record(device, cb);
            }

            // ---- DFG LUT（一次性）：READ_ONLY → GENERAL → dispatch → READ_ONLY ----
            if plan.dfg {
                ImageBarrier::transition(
                    self.dfg.image.image,
                    color,
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::GENERAL,
                    ),
                    (read_stage, vk::PipelineStageFlags::COMPUTE_SHADER),
                    (vk::AccessFlags::SHADER_READ, vk::AccessFlags::SHADER_WRITE),
                )
                .record(device, cb);
                // 二维派发：workgroup 8×8，两个轴都要铺满 128（一维派发只会写第一行带）
                let dfg = self.handles(&self.brdf_lut);
                dfg.bind(device, cb, &[DescSet::new(&IBL_DFG_SET_DESC, self.dfg_set)]);
                let groups = DFG_LUT_SIZE.div_ceil(8);
                device.cmd_dispatch(cb, groups, groups, 1);
                Handoff::ComputeRead.barrier().record(device, cb);
                ImageBarrier::transition(
                    self.dfg.image.image,
                    color,
                    (
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    ),
                    (vk::PipelineStageFlags::COMPUTE_SHADER, read_stage),
                    (vk::AccessFlags::SHADER_WRITE, vk::AccessFlags::SHADER_READ),
                )
                .record(device, cb);
            }

            if !plan.capture {
                return;
            }

            // ---- 捕获链：全子资源进 GENERAL（src 是上一轮的采样读，WAR）----
            ImageBarrier::transition(
                self.cube.image,
                color,
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                ),
                (read_stage, vk::PipelineStageFlags::COMPUTE_SHADER),
                (vk::AccessFlags::SHADER_READ, vk::AccessFlags::SHADER_WRITE),
            )
            .with_range(0, IBL_CUBE_MIPS, 0, CUBE_LAYERS)
            .record(device, cb);
            // 缓冲的跨帧 WAR：上一帧的合成/透明（片元）与本链的 compute 读要排在下面的
            // 写之前——缓冲没有布局迁移可搭车，必须单独一条内存屏障（图像侧由上面那条
            // 布局迁移的 src 域覆盖）。
            MemoryBarrierDesc {
                src_stage: read_stage,
                src_access: vk::AccessFlags::SHADER_READ,
                dst_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                dst_access: vk::AccessFlags::SHADER_WRITE,
            }
            .record(device, cb);

            // 1) 天空 → mip0：每面一个 z 片，(32, 32, 6) 一次派发（ComputeHandles 只发一维，
            //    这里按三维直调）
            let capture = self.handles(&self.capture);
            capture.bind(
                device,
                cb,
                &[DescSet::new(&IBL_CAPTURE_SET_DESC, self.capture_sets[slot])],
            );
            let face_groups = IBL_CUBE_SIZE.div_ceil(8);
            device.cmd_dispatch(cb, face_groups, face_groups, CUBE_LAYERS);

            // 2) mip0 写 → mip0 读（同子资源 RAW；只有 mip0 参与，别在这里转全链）
            ImageBarrier::transition(
                self.cube.image,
                color,
                (vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL),
                (
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                ),
                (vk::AccessFlags::SHADER_WRITE, vk::AccessFlags::SHADER_READ),
            )
            .with_range(0, 1, 0, CUBE_LAYERS)
            .record(device, cb);

            // 3) SH 投影 → 归约 → 自检（串行依赖，各自 dispatch 后带交班屏障）。
            //    自检后 SH 系数要进片元，交班到 compute|fragment。
            self.handles(&self.sh_project).record(
                device,
                cb,
                &[DescSet::new(&IBL_SH_PROJECT_SET_DESC, self.project_set)],
                &[],
                1,
                Handoff::ComputeRead,
            );
            self.handles(&self.sh_reduce).record(
                device,
                cb,
                &[DescSet::new(&IBL_SH_REDUCE_SET_DESC, self.reduce_set)],
                &[],
                1,
                Handoff::ComputeRead,
            );
            self.handles(&self.sh_check).record(
                device,
                cb,
                &[DescSet::new(&IBL_SH_CHECK_SET_DESC, self.check_set)],
                &[],
                1,
                Handoff::ComputeFragmentRead,
            );
            // 3b) GlobalProbe 16 桶：只读 mip0、写 probe buffer；与 SH 同批重算。
            //     16 个线程各认领一个桶（workgroup 16、一次 dispatch）。
            self.handles(&self.global_probe).record(
                device,
                cb,
                &[DescSet::new(&IBL_GLOBAL_PROBE_SET_DESC, self.probe_set)],
                &[],
                1,
                Handoff::ComputeRead,
            );

            // 4) GGX 预滤波 mip1..8（每级一个 dispatch；源只读 mip0，级间不同子资源）
            let prefilter = self.handles(&self.prefilter);
            for (index, &samples) in PREFILTER_SAMPLES.iter().enumerate() {
                let mip = index as u32 + 1;
                let size = (IBL_CUBE_SIZE >> mip).max(1);
                let perceptual_roughness = mip as f32 / IBL_MAX_MIP;
                let mut push = [0u8; PREFILTER_PUSH_SIZE as usize];
                push[0..4].copy_from_slice(&perceptual_roughness.to_ne_bytes());
                push[4..8].copy_from_slice(&samples.to_ne_bytes());
                prefilter.bind(
                    device,
                    cb,
                    &[DescSet::new(
                        &IBL_PREFILTER_SET_DESC,
                        self.prefilter_sets[index],
                    )],
                );
                device.cmd_push_constants(
                    cb,
                    prefilter.layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    &push,
                );
                // 三维 dispatch：z = 6 面（handles.dispatch 只发一维）
                device.cmd_dispatch(cb, size.div_ceil(8), size.div_ceil(8), CUBE_LAYERS);
            }
            // 预滤波写 mip1..8 与 SH/stats 的写 → 采样读（片元）收口；图像另由下面
            // 的全子资源布局迁移提供 RAW（迁移的 src 域覆盖前面所有写）
            Handoff::ComputeFragmentRead.barrier().record(device, cb);
            ImageBarrier::transition(
                self.cube.image,
                color,
                (
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                ),
                (vk::PipelineStageFlags::COMPUTE_SHADER, read_stage),
                (vk::AccessFlags::SHADER_WRITE, vk::AccessFlags::SHADER_READ),
            )
            .with_range(0, IBL_CUBE_MIPS, 0, CUBE_LAYERS)
            .record(device, cb);
        }
    }

    /// 读回环境 cubemap 的某一级（六面，原始 RGBA16F → f32，行主序）。诊断用：
    /// `ibl_census` 验面方向/接缝/非有限值，低频调用（每面一次一次性提交）。
    ///
    /// # Errors
    /// 一次性 staging / 命令池 / 提交 / 映射失败。
    pub fn readback_cube(&self, ctx: &Context, mip: u32) -> Result<Vec<Vec<f32>>> {
        let size = (IBL_CUBE_SIZE >> mip).max(1);
        let byte_size = u64::from(size) * u64::from(size) * 8;
        let mut faces = Vec::with_capacity(CUBE_LAYERS as usize);
        for layer in 0..CUBE_LAYERS {
            let mut bytes = vec![0u8; byte_size as usize];
            unsafe {
                let readback = OneShotReadback::begin(ctx, byte_size)?;
                // 采样态 → TRANSFER_SRC → 回采样态（调用时机 = BRP，cubemap 恒在采样态）
                ImageBarrier::transition(
                    self.cube.image,
                    vk::ImageAspectFlags::COLOR,
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ),
                    (
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::PipelineStageFlags::TRANSFER,
                    ),
                    (
                        vk::AccessFlags::MEMORY_WRITE,
                        vk::AccessFlags::TRANSFER_READ,
                    ),
                )
                .with_range(mip, 1, layer, 1)
                .record(&ctx.device, readback.cb);
                let region = vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(mip)
                            .base_array_layer(layer)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: size,
                        height: size,
                        depth: 1,
                    });
                ctx.device.cmd_copy_image_to_buffer(
                    readback.cb,
                    self.cube.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback.staging,
                    std::slice::from_ref(&region),
                );
                ImageBarrier::transition(
                    self.cube.image,
                    vk::ImageAspectFlags::COLOR,
                    (
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    ),
                    (
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::COMPUTE_SHADER
                            | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    ),
                    (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::SHADER_READ),
                )
                .with_range(mip, 1, layer, 1)
                .record(&ctx.device, readback.cb);
                readback.finish_and_read(ctx, &mut bytes)?;
            }
            faces.push(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|w| half_to_f32(u16::from_le_bytes(*w)))
                    .collect(),
            );
        }
        Ok(faces)
    }

    /// 读回 DFG LUT（RGBA16F → f32，行主序，x = NoV、y = p）。诊断用：验生成结果
    /// 无 NaN/Inf，且 A/B 落在合理量级。
    ///
    /// # Errors
    /// 一次性 staging / 命令池 / 提交 / 映射失败。
    pub fn readback_dfg(&self, ctx: &Context) -> Result<Vec<f32>> {
        let byte_size = u64::from(DFG_LUT_SIZE) * u64::from(DFG_LUT_SIZE) * 8;
        let mut bytes = vec![0u8; byte_size as usize];
        unsafe {
            let readback = OneShotReadback::begin(ctx, byte_size)?;
            ImageBarrier::transition(
                self.dfg.image.image,
                vk::ImageAspectFlags::COLOR,
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                ),
                (
                    vk::AccessFlags::MEMORY_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                ),
            )
            .record(&ctx.device, readback.cb);
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: DFG_LUT_SIZE,
                    height: DFG_LUT_SIZE,
                    depth: 1,
                });
            ctx.device.cmd_copy_image_to_buffer(
                readback.cb,
                self.dfg.image.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.staging,
                std::slice::from_ref(&region),
            );
            ImageBarrier::transition(
                self.dfg.image.image,
                vk::ImageAspectFlags::COLOR,
                (
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER
                        | vk::PipelineStageFlags::FRAGMENT_SHADER,
                ),
                (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::SHADER_READ),
            )
            .record(&ctx.device, readback.cb);
            readback.finish_and_read(ctx, &mut bytes)?;
        }
        Ok(bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|w| half_to_f32(u16::from_le_bytes(*w)))
            .collect())
    }

    /// 取内部管线句柄。`None` 只出现在 `Drop` 之后（录制不可达）——录制期缺句柄是
    /// 状态矛盾，按契约错误直接 panic，不静默跳过。
    fn handles(&self, pipeline: &Option<ComputePipeline>) -> ComputeHandles {
        match pipeline {
            Some(p) => p.handles(),
            None => panic!("IBL 管线在录制期已缺席（资源被提前拆？）"),
        }
    }
}

impl Drop for IblEnvironment {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。
        // 序：管线（引用 set 布局）→ 池（含 set）→ set 布局 → 采样器；视图/图随字段拆。
        self.capture = None;
        self.sh_project = None;
        self.sh_reduce = None;
        self.sh_check = None;
        self.global_probe = None;
        self.prefilter = None;
        self.brdf_lut = None;
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_pool(self.frame_pool, None);
            for layout in [
                self.capture_layout,
                self.project_layout,
                self.reduce_layout,
                self.check_layout,
                self.probe_layout,
                self.prefilter_layout,
                self.dfg_layout,
                self.frame_layout,
            ] {
                self.device.destroy_descriptor_set_layout(layout, None);
            }
            self.device.destroy_sampler(self.sampler, None);
        }
    }
}

/// `sky_hash`：把影响捕获结果的输入 f32 位模式首尾相接地滚一遍 FNV-1a。
/// 含：全部 `SkySettings` 数值字段、常量天空开关与颜色、方向光方向/颜色、
/// 按 100 m 量化的相机海拔。不含 `sky_enabled`（可见天空的开关不改变环境项）。
fn sky_hash(input: &IblBakeInput<'_>) -> u64 {
    let sky = input.sky;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: f32| {
        for b in v.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(sky.intensity);
    eat(sky.planet_radius);
    eat(sky.atmosphere_height);
    for c in sky.rayleigh_scattering {
        eat(c);
    }
    eat(sky.rayleigh_scale_height);
    eat(sky.mie_scattering);
    eat(sky.mie_scale_height);
    eat(sky.mie_anisotropy);
    for c in sky.ozone_absorption {
        eat(c);
    }
    eat(sky.ozone_center);
    eat(sky.ozone_width);
    for c in sky.ground_albedo {
        eat(c);
    }
    eat(sky.sun_gain);
    eat(sky.sun_radius);
    eat(if sky.uniform_enabled { 1.0 } else { 0.0 });
    for c in sky.uniform_radiance {
        eat(c);
    }
    for c in input.dir_to_light {
        eat(c);
    }
    for c in input.light_color {
        eat(c);
    }
    // 海拔：按 100 m 量化后只吃整数部分（地面漫游的米级差永不触发重烘）
    eat((input.altitude / ALTITUDE_HASH_STEP).floor());
    h
}

/// 建一张 OPTIMAL tiling 的 2D 图并按需分配 dedicated memory。
fn create_image(
    ctx: &Context,
    extent: vk::Extent3D,
    mip_levels: u32,
    array_layers: u32,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
    label: &str,
) -> Result<IblImage> {
    let device = &ctx.device;
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .flags(flags)
                .image_type(vk::ImageType::TYPE_2D)
                .format(IBL_FORMAT)
                .extent(extent)
                .mip_levels(mip_levels)
                .array_layers(array_layers)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }?;
    let result = unsafe {
        let reqs = device.get_image_memory_requirements(image);
        let memory_type = ctx.memory_contract().find_type(
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        );
        memory_type.and_then(|memory_type| {
            let memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            device.bind_image_memory(image, memory, 0)?;
            Ok(IblImage {
                device: device.clone(),
                image,
                memory,
            })
        })
    };
    match result {
        Ok(image) => {
            ctx.name_object(image.image, &format!("ibl_image: {label}"));
            Ok(image)
        }
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            Err(e)
        }
    }
}

/// 环境 cubemap 的三类视图（全链 cube / mip0 cube / 每级 2D-array）。
fn build_cube_views(device: &Device, image: vk::Image) -> Result<CubeViews> {
    let mut views = CubeViews {
        device: device.clone(),
        full: vk::ImageView::null(),
        mip0: vk::ImageView::null(),
        storage: Vec::with_capacity(IBL_CUBE_MIPS as usize),
    };
    views.full = unsafe { create_cube_view(device, image, 0, IBL_CUBE_MIPS) }?;
    views.mip0 = unsafe { create_cube_view(device, image, 0, 1) }?;
    for mip in 0..IBL_CUBE_MIPS {
        views
            .storage
            .push(unsafe { create_array_view(device, image, mip) }?);
    }
    Ok(views)
}

/// 2D-array 视图（`base_mip` 起一级，六层）——捕获与预滤波的 storage 目标。
///
/// # Safety
/// `image` 须是合法句柄且寿命覆盖视图。
unsafe fn create_array_view(
    device: &Device,
    image: vk::Image,
    base_mip: u32,
) -> Result<vk::ImageView> {
    unsafe {
        create_view(
            device,
            image,
            vk::ImageViewType::TYPE_2D_ARRAY,
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(base_mip)
                .level_count(1)
                .layer_count(CUBE_LAYERS),
        )
    }
}

/// cube 视图（`base_mip` 起 `level_count` 级、六层）。
///
/// # Safety
/// `image` 须是合法句柄且寿命覆盖视图。
unsafe fn create_cube_view(
    device: &Device,
    image: vk::Image,
    base_mip: u32,
    level_count: u32,
) -> Result<vk::ImageView> {
    unsafe {
        create_view(
            device,
            image,
            vk::ImageViewType::CUBE,
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(base_mip)
                .level_count(level_count)
                .layer_count(CUBE_LAYERS),
        )
    }
}

/// 单视图创建的公共段（view type + 子资源范围由调用方给）。
///
/// # Safety
/// 同 [`create_cube_view`]。
unsafe fn create_view(
    device: &Device,
    image: vk::Image,
    view_type: vk::ImageViewType,
    range: vk::ImageSubresourceRange,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(view_type)
        .format(IBL_FORMAT)
        .subresource_range(range);
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

/// fp16 → f32（读回诊断用；含次正规数、Inf/NaN 直通）。纯位搬移，不做浮点运算。
pub fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = u32::from(bits >> 10) & 0x1f;
    let mant = u32::from(bits & 0x03ff);
    let out = match exp {
        // 次正规（含零）：value = mant·2^-24，规范化为 f32 正规数
        0 => {
            if mant == 0 {
                sign
            } else {
                let k = 31 - mant.leading_zeros(); // 最高位位置 0..9
                sign | ((103 + k) << 23) | ((mant << (23 - k)) & 0x7f_ffff)
            }
        }
        // Inf / NaN：指数全 1，尾数搬位保留 NaN 性
        0x1f => sign | 0x7f80_0000 | (mant << 13),
        // 正规数：f32 指数 = exp − 15 + 127 = exp + 112
        _ => sign | ((exp + 112) << 23) | (mant << 13),
    };
    f32::from_bits(out)
}
