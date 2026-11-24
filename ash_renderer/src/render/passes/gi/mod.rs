//! 表面缓存全局光照（9.5）：Volume/cell 格子系统、patch 池与分配回收、patch 调试视图。
//!
//! 本段只建池与生命周期（位置/法线/计数/回收），不出辐照度（9.6）、不接屏幕查询（9.7）；
//! `GiSettings::enabled = false`（默认）= 不录任何 dispatch、不建视图，画面与 9.3 逐位一致。
//!
//! 帧内顺序（④.5 段，G-buffer 转只读之后、合成之前）：
//! `scrolling → eviction → defrag × waves → patch_alloc`，再加一道 compute→片元屏障给
//! ⑧ 段视图读。先腾空间再分配，同帧被释放的槽本帧播种即可复用。
//!
//! 子模块：[`volume`] 格子系统 CPU 镜像；[`allocation`] 播种 dispatch 与录制束；
//! [`view`] 调试视图两形态。常驻表与 GI UBO 在 `resident/tables/surface_cache.rs`。

pub mod allocation;
pub mod estimation;
pub mod view;
pub mod volume;

use ash::{vk, Device};

use crate::common::error::Result;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::passes::ibl::IBL_SET_DESC;
use crate::render::passes::post::smaa::LDR_FORMAT;
use crate::render::passes::ray_tracing::RAY_TRACING_SET_DESC;
use crate::render::passes::shadow::shadow_map::SHADOW_SET_DESC;
use crate::render::resident::tables::bindless_texture_table::BINDLESS_TEXTURE_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::resident::tables::surface_cache::GI_SET_DESC;
use crate::render::Context;

pub use self::allocation::GiAllocDraw;
pub use self::estimation::{GiEmissiveBuildDraw, GiEstimationDraw, GiLightSampleDraw};
pub use self::view::GiViewDraw;
pub use self::volume::SurfaceCacheConfig;

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 淘汰年龄门限（帧）：心跳超过它就回收（60 fps 下 4 秒，Unity 同值 240）。
pub const EVICT_AGE: u32 = 240;
/// 9.6 的时域更新目标次数（本段只写进 UBO，不使用）。
pub const UPDATE_MAX: u32 = 32;

/// 表面缓存设置（宿主真源；BRP `gi_set` 写）。
#[derive(Clone, Copy, bevy::prelude::Resource)]
pub struct GiSettings {
    /// 总开关：关 = 不录 dispatch、不建视图（画面与 9.3 逐位一致）。
    pub enabled: bool,
    /// 调试视图号：0 = 关；1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 / 6 池水位 /
    /// 7 位置偏离 / 8 cell 归属。
    pub view: u32,
    /// 视图形态：0 = 覆盖图（主）；1 = 覆盖图 + 方片；2 = 只方片。
    pub form: u32,
    /// 法线来源：0 = 深度差分平面法线（默认）；1 = G-buffer 着色法线（A/B 对照）。
    pub normal_source: u32,
    /// 播种开关（0 = 只回收不播种，A/B 用）。
    pub seed: bool,
    /// 每帧整理波数（0 = 不整理；每波 512 条）。
    pub defrag_waves: u32,
    /// 9.6 估计开关：关 = 不录 estimation dispatch，patch 表只有 IBL 种子
    /// （与 9.5 收官画面逐字节一致；默认关）。
    pub estimation: bool,
    /// 9.6 每 patch 每帧样本档（1/2/4/8）。预算不足时宿主按 §9.6.5 降到下界 1 再写 UBO。
    pub samples: u32,
    /// 9.6 预热倍率（1/2/4/8）：`update_count == 0` 且 rank 0 的 patch 一次吃这么多倍样本。
    pub warmup: u32,
    /// 9.6 方差引导时域混合的短时滞后（0.5～0.99；越小越跟手、越大越稳）。
    pub short_hysteresis: f32,
    /// 9.6 多跳开关：命中点查自己所属 cell 的 patch（读上一帧）。
    pub multi_bounce: bool,
    /// 9.6 每帧射线预算（射线数 = 活跃 patch × 样本档 × 每样本射线数）。
    pub budget: u32,
    /// 9.6 自发光三角形强度倍数（Unity `_EmissiveTriangleIntensityMultiplier` 同款）。
    pub emissive_intensity: f32,
    /// 9.6 光源侧采样清单长度（每帧从灯位向锥内采样的条数）。
    pub light_samples: u32,
    /// Volume/池规格（只在关态改；改后重建表）。
    pub config: SurfaceCacheConfig,
}

impl Default for GiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            view: 0,
            form: 0,
            normal_source: 0,
            seed: true,
            defrag_waves: 2,
            estimation: false,
            samples: 2,
            warmup: 4,
            short_hysteresis: 0.85,
            multi_bounce: true,
            budget: 262_144,
            emissive_intensity: 1.0,
            light_samples: 128,
            config: SurfaceCacheConfig::default(),
        }
    }
}

impl GiSettings {
    /// 视图是否要画覆盖图（形态 A）。
    #[must_use]
    pub fn cover_enabled(&self) -> bool {
        self.enabled && self.view > 0 && self.form != 2
    }

    /// 视图是否要画方片（形态 B）。
    #[must_use]
    pub fn markers_enabled(&self) -> bool {
        self.enabled && self.view > 0 && self.form >= 1
    }
}

/// 滚动释放（set0 = GI 集，无 push；线程 = 级联数 × res³）。
pub(crate) static SCROLLING_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_scrolling",
    shader: &shader_abi::SCROLLING,
    entries: &[c"cs_main"],
    sets: &[&GI_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 心跳淘汰（set0 = GI 集；线程 = 池容量）。
pub(crate) static EVICTION_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_eviction",
    shader: &shader_abi::EVICTION,
    entries: &[c"cs_main"],
    sets: &[&GI_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 整理（set0 = GI 集；每次 dispatch 一个 workgroup / 一波 512 条）。
pub(crate) static DEFRAG_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_defrag",
    shader: &shader_abi::DEFRAG,
    entries: &[c"cs_main"],
    sets: &[&GI_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 播种（set0 帧 UBO / set1 G-buffer / set2 IBL / set3 GI + 32B push）。
pub(crate) static PATCH_ALLOC_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_patch_alloc",
    shader: &shader_abi::PATCH_ALLOC,
    entries: &[c"cs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &IBL_SET_DESC,
        &GI_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: allocation::GI_ALLOC_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 9.6 estimation（set 序 = 帧 UBO / GI 表 / 实例表 / 光线求交集 / 阴影集 / bindless）。
///
/// 只在光线求交支持且开启的机器上建管线：含 `RayQueryKHR` 的 shader 在非光线求交设备上
/// 建不了管线，塞进恒建清单会破坏"关闭 = 逐字节一致"的降级路径（9.4 同款教训）。
/// 光线求交集是**复用** 9.4 那一份（b0 TLAS / b1 顶点池 / b2 索引池），本 shader 不声明
/// 它的 b3 调试图与 b4 统计。
pub(crate) static ESTIMATION_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_estimation",
    shader: &shader_abi::ESTIMATION,
    entries: &[c"cs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GI_SET_DESC,
        &INSTANCE_SET_DESC,
        &RAY_TRACING_SET_DESC,
        &SHADOW_SET_DESC,
        &BINDLESS_TEXTURE_SET_DESC,
    ],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 9.6.2 光源侧采样清单（set 序 = GI / 实例 / 光线求交 / bindless + 8B push）。
///
/// 与 estimation 同样只在光线求交支持的机器上建；它的交换屏障是 compute→compute，
/// 下游 estimation 同段录制，屏障随 `ComputeReadWrite` 走。
pub(crate) static LIGHT_SAMPLING_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_light_sampling",
    shader: &shader_abi::LIGHT_SAMPLING,
    entries: &[c"cs_main"],
    sets: &[
        &GI_SET_DESC,
        &INSTANCE_SET_DESC,
        &RAY_TRACING_SET_DESC,
        &BINDLESS_TEXTURE_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: estimation::GI_LIGHT_SAMPLING_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 9.6.2 自发光三角形表重建（set 序 = GI / 实例 / 光线求交 / bindless + 4B push）。
///
/// 每帧重建：按对象槽展开自发光三角形，条数记进 `counters[14]`。只在光线求交支持的
/// 机器上建（顶点/索引池要带 storage）。
pub(crate) static EMISSIVE_BUILD_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_emissive_build",
    shader: &shader_abi::EMISSIVE_BUILD,
    entries: &[c"cs_main"],
    sets: &[
        &GI_SET_DESC,
        &INSTANCE_SET_DESC,
        &RAY_TRACING_SET_DESC,
        &BINDLESS_TEXTURE_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: estimation::GI_EMISSIVE_BUILD_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 覆盖图（形态 A）：全屏三角形，颜色附件 = LDR 目标；16B push（视图/法线来源/宽高）。
pub(crate) static PATCH_VIEW_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_patch_view",
    shader: &shader_abi::PATCH_VIEW,
    entries: &[c"vs_main", c"fs_cover"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &GI_SET_DESC,
    ],
    push: Some(PushRange {
        stages: crate::render::core::binding::set_layout::stages(&[
            vk::ShaderStageFlags::VERTEX,
            vk::ShaderStageFlags::FRAGMENT,
        ]),
        size: view::GI_VIEW_PUSH_SIZE,
    }),
    color_formats: &[LDR_FORMAT],
    depth_format: None,
};

/// 方片（形态 B）：实例化 6 顶点 × 池容量，人工深度测试；同一份 sets/push。
pub(crate) static PATCH_MARKER_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gi_patch_marker",
    shader: &shader_abi::PATCH_VIEW,
    entries: &[c"vs_marker", c"fs_marker"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &GI_SET_DESC,
    ],
    push: Some(PushRange {
        stages: crate::render::core::binding::set_layout::stages(&[
            vk::ShaderStageFlags::VERTEX,
            vk::ShaderStageFlags::FRAGMENT,
        ]),
        size: view::GI_VIEW_PUSH_SIZE,
    }),
    color_formats: &[LDR_FORMAT],
    depth_format: None,
};

/// 表面缓存的七条管线（进程级；estimation 只在光线求交支持的机器上建）。
#[derive(bevy::prelude::Resource)]
pub struct GiPipelines {
    scrolling: ComputePipeline,
    eviction: ComputePipeline,
    defrag: ComputePipeline,
    alloc: ComputePipeline,
    estimation: Option<ComputePipeline>,
    light_sampling: Option<ComputePipeline>,
    emissive_build: Option<ComputePipeline>,
    cover: FullscreenPipeline,
    marker: FullscreenPipeline,
}

impl GiPipelines {
    /// `ray_tracing` = 本机支持并开启了光线求交：只决定 estimation 管线建不建
    /// （其余六条与 9.5 一致）。
    ///
    /// # Errors
    /// 配方对账不过或管线创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts, ray_tracing: bool) -> Result<Self> {
        let estimation = ray_tracing
            .then(|| ComputePipeline::new(ctx, &ESTIMATION_PIPELINE, layouts))
            .transpose()?;
        let light_sampling = ray_tracing
            .then(|| ComputePipeline::new(ctx, &LIGHT_SAMPLING_PIPELINE, layouts))
            .transpose()?;
        let emissive_build = ray_tracing
            .then(|| ComputePipeline::new(ctx, &EMISSIVE_BUILD_PIPELINE, layouts))
            .transpose()?;
        Ok(Self {
            scrolling: ComputePipeline::new(ctx, &SCROLLING_PIPELINE, layouts)?,
            eviction: ComputePipeline::new(ctx, &EVICTION_PIPELINE, layouts)?,
            defrag: ComputePipeline::new(ctx, &DEFRAG_PIPELINE, layouts)?,
            alloc: ComputePipeline::new(ctx, &PATCH_ALLOC_PIPELINE, layouts)?,
            estimation,
            light_sampling,
            emissive_build,
            cover: FullscreenPipeline::new(ctx, &PATCH_VIEW_PIPELINE, layouts)?,
            marker: FullscreenPipeline::new(ctx, &PATCH_MARKER_PIPELINE, layouts)?,
        })
    }

    #[must_use]
    pub fn scrolling(&self) -> ComputeHandles {
        self.scrolling.handles()
    }

    #[must_use]
    pub fn eviction(&self) -> ComputeHandles {
        self.eviction.handles()
    }

    #[must_use]
    pub fn defrag(&self) -> ComputeHandles {
        self.defrag.handles()
    }

    #[must_use]
    pub fn alloc(&self) -> ComputeHandles {
        self.alloc.handles()
    }

    /// 9.6 estimation 的录制句柄（None = 本机不支持光线求交）。
    #[must_use]
    pub fn estimation(&self) -> Option<ComputeHandles> {
        self.estimation.as_ref().map(ComputePipeline::handles)
    }

    /// 9.6.2 光源侧采样清单的录制句柄（None = 本机不支持光线求交）。
    #[must_use]
    pub fn light_sampling(&self) -> Option<ComputeHandles> {
        self.light_sampling.as_ref().map(ComputePipeline::handles)
    }

    /// 9.6.2 自发光三角形表重建的录制句柄（None = 本机不支持光线求交）。
    #[must_use]
    pub fn emissive_build(&self) -> Option<ComputeHandles> {
        self.emissive_build.as_ref().map(ComputePipeline::handles)
    }

    #[must_use]
    pub fn cover(&self) -> crate::render::core::record::graphics::PipelineRef {
        self.cover.rec()
    }

    #[must_use]
    pub fn marker(&self) -> crate::render::core::record::graphics::PipelineRef {
        self.marker.rec()
    }
}

/// ④.5 段录制束（GI 关 = None，整段跳过）。
pub struct GiFrameDraw {
    /// 滚动释放（只在级联偏移变化的帧为 Some）。
    pub scrolling: Option<ComputeHandles>,
    /// 滚动 dispatch 组数（级联数 × res³ / 64）。
    pub scrolling_groups: u32,
    /// 心跳淘汰。
    pub eviction: ComputeHandles,
    pub eviction_groups: u32,
    /// 整理（每帧 `defrag_waves` 次、每次一个 workgroup 串行推进续扫游标）。
    pub defrag: ComputeHandles,
    pub defrag_waves: u32,
    /// 屏幕播种（播种关 / 无相机的帧为 None）。
    pub alloc: Option<GiAllocDraw>,
    /// 9.6.2 自发光三角形表重建（估计关 / 无光线求交的帧为 None；每帧重建）。
    pub emissive_build: Option<GiEmissiveBuildDraw>,
    /// 9.6.2 光源侧采样清单（估计关 / 无灯 / 无光线求交的帧为 None）。
    pub light_sampling: Option<GiLightSampleDraw>,
    /// 9.6 estimation（估计关 / 无光线求交 / 无实例的帧为 None）。
    pub estimation: Option<GiEstimationDraw>,
    /// 本帧槽 GI 集（回收三件与播种共绑）。
    pub gi_set: DescSet,
    /// ⑧ 段调试视图（关 / 视图 0 为 None）。
    pub view: Option<GiViewDraw>,
}

impl GiFrameDraw {
    /// 录 ④.5 段：scrolling → eviction → defrag × waves → alloc，末道 compute→片元屏障。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        if let Some(scrolling) = &self.scrolling {
            scrolling.record(
                device,
                cb,
                &[self.gi_set],
                &[],
                self.scrolling_groups,
                Handoff::ComputeReadWrite,
            );
        }
        self.eviction.record(
            device,
            cb,
            &[self.gi_set],
            &[],
            self.eviction_groups,
            Handoff::ComputeReadWrite,
        );
        for _ in 0..self.defrag_waves {
            self.defrag.record(
                device,
                cb,
                &[self.gi_set],
                &[],
                1,
                Handoff::ComputeReadWrite,
            );
        }
        if let Some(alloc) = &self.alloc {
            alloc.record(device, cb, self.gi_set);
        }
        // 9.6.2：自发光表重建 → 清单 → estimation（后两者都读前者的产出）
        if let Some(emissive_build) = &self.emissive_build {
            emissive_build.record(device, cb);
        }
        if let Some(light_sampling) = &self.light_sampling {
            light_sampling.record(device, cb);
        }
        // 9.6：播种之后、段末屏障之前——estimation 读本帧刚刷新/分配的 patch 几何
        if let Some(estimation) = &self.estimation {
            estimation.record(device, cb);
        }
        // 缓存表 compute 写 → ⑧ 段片元读（视图）与 9.7 的片元查表
        unsafe { Handoff::ComputeFragmentRead.barrier().record(device, cb) };
    }
}
