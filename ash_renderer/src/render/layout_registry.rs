//! 布局契约登记表：宿主建的全部管线配方与 set 声明的全量清单。新增管线/set
//! 必须登记在这里，否则契约测试 `every_shader_entry_has_a_pipeline` 和
//! `ash_renderer/layout_contract`（BRP 报表）会报。
//!
//! 这个模块不挂 `cfg(test)`：BRP 只读方法在运行期也要读它。

use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayoutDesc;
use crate::render::passes::gbuffer::gbuffer_pipeline::GBUFFER_WRITE_PIPELINE;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::passes::gi::screen_gather::SCGI_TARGET_SET_DESC;
use crate::render::passes::gi::{
    BOUNCE_ALLOC_PIPELINE, DEFRAG_PIPELINE, EMISSIVE_BUILD_PIPELINE, ESTIMATION_PIPELINE,
    EVICTION_PIPELINE, LIGHT_SAMPLING_PIPELINE, PATCH_ALLOC_PIPELINE, PATCH_MARKER_PIPELINE,
    PATCH_VIEW_PIPELINE, SCREEN_GATHER_PIPELINE, SCROLLING_PIPELINE, SPATIAL_FILTER_PIPELINE,
    TEMPORAL_FILTER_PIPELINE,
};

use crate::render::passes::ibl::{
    BRDF_LUT_PIPELINE, GLOBAL_PROBE_PIPELINE, IBL_CAPTURE_SET_DESC, IBL_DFG_SET_DESC,
    IBL_GLOBAL_PROBE_SET_DESC, IBL_PREFILTER_SET_DESC, IBL_SET_DESC, IBL_SH_CHECK_SET_DESC,
    IBL_SH_PROJECT_SET_DESC, IBL_SH_REDUCE_SET_DESC, PREFILTER_PIPELINE, SH_CHECK_PIPELINE,
    SH_PROJECT_PIPELINE, SH_REDUCE_PIPELINE, SKY_CAPTURE_PIPELINE,
};
use crate::render::passes::lighting::cluster::{CLUSTER_BUILD_PIPELINE, LIGHT_CLUSTER_SET_DESC};
use crate::render::passes::lighting::composite_pipeline::GBUFFER_COMPOSITE_PIPELINE;
use crate::render::passes::overlay::overlay_pipeline::OVERLAY_PIPELINE;
use crate::render::passes::post::bloom::{
    BLOOM_DOWN_PIPELINE, BLOOM_PREFILTER_PIPELINE, BLOOM_TEX_SET_DESC, BLOOM_UP_PIPELINE,
    BLOOM_UP_SET_DESC,
};
use crate::render::passes::post::present_tonemap_pipeline::PRESENT_TONEMAP_PIPELINE;
use crate::render::passes::post::smaa::{
    SMAA_A_SET_DESC, SMAA_BLEND_PIPELINE, SMAA_B_SET_DESC, SMAA_EDGE_PIPELINE,
    SMAA_NEIGHBORHOOD_PIPELINE,
};
use crate::render::passes::ray_tracing::{
    RAY_DEBUG_PIPELINE, RAY_TRACING_SET_DESC, TLAS_BUILD_PIPELINE,
};
use crate::render::passes::shadow::shadow_build::SHADOW_BUILD_PIPELINE;
use crate::render::passes::shadow::shadow_map::{
    SHADOW_SET_DESC, SHADOW_WRITE_MASK_PIPELINE, SHADOW_WRITE_PIPELINE,
};
use crate::render::passes::transparent::{
    TRANSPARENT_COLLECT_PIPELINE, TRANSPARENT_FORWARD_PIPELINE, TRANSPARENT_GATHER_PIPELINE,
    TRANSPARENT_SET_DESC, TRANSPARENT_SORT_PIPELINE,
};
use crate::render::passes::visibility::cull::CULL_PIPELINE;
use crate::render::passes::visibility::hiz::{
    HIZ_DOWNSAMPLE_PIPELINE, HIZ_OCCLUSION_SET_DESC, HIZ_REDUCE_SET_DESC,
};
use crate::render::passes::visibility::indirect::BUILD_INDIRECT_PIPELINE;
use crate::render::passes::visibility::lod::LOD_SELECT_PIPELINE;
use crate::render::passes::visibility::occlusion::OCCLUSION_PIPELINE;
use crate::render::resident::tables::bindless_texture_table::BINDLESS_TEXTURE_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::resident::tables::surface_cache::GI_SET_DESC;

/// 宿主建的全部管线。
pub(crate) const ALL_PIPELINES: &[&PipelineDesc] = &[
    &GBUFFER_WRITE_PIPELINE,
    &SHADOW_WRITE_PIPELINE,
    &SHADOW_WRITE_MASK_PIPELINE,
    &TRANSPARENT_FORWARD_PIPELINE,
    &OVERLAY_PIPELINE,
    &GBUFFER_COMPOSITE_PIPELINE,
    &BLOOM_PREFILTER_PIPELINE,
    &BLOOM_DOWN_PIPELINE,
    &BLOOM_UP_PIPELINE,
    &SMAA_EDGE_PIPELINE,
    &SMAA_BLEND_PIPELINE,
    &SMAA_NEIGHBORHOOD_PIPELINE,
    &PRESENT_TONEMAP_PIPELINE,
    &CULL_PIPELINE,
    &BUILD_INDIRECT_PIPELINE,
    &SHADOW_BUILD_PIPELINE,
    &OCCLUSION_PIPELINE,
    &LOD_SELECT_PIPELINE,
    &CLUSTER_BUILD_PIPELINE,
    &TRANSPARENT_COLLECT_PIPELINE,
    &TRANSPARENT_SORT_PIPELINE,
    &TRANSPARENT_GATHER_PIPELINE,
    &HIZ_DOWNSAMPLE_PIPELINE,
    &SKY_CAPTURE_PIPELINE,
    &SH_PROJECT_PIPELINE,
    &SH_REDUCE_PIPELINE,
    &SH_CHECK_PIPELINE,
    &GLOBAL_PROBE_PIPELINE,
    &PREFILTER_PIPELINE,
    &BRDF_LUT_PIPELINE,
    // 光线求交：主机只在 光线求交支持时建（登记表恒列，契约测试按清单核对）
    &TLAS_BUILD_PIPELINE,
    &RAY_DEBUG_PIPELINE,
    // 表面缓存（9.5）：回收三件 + 播种 + 调试视图两形态
    &SCROLLING_PIPELINE,
    &EVICTION_PIPELINE,
    &DEFRAG_PIPELINE,
    &PATCH_ALLOC_PIPELINE,
    &PATCH_VIEW_PIPELINE,
    &PATCH_MARKER_PIPELINE,
    // 9.6 估计：只在光线求交支持的机器上建（登记表恒列，契约测试按清单核对）
    &ESTIMATION_PIPELINE,
    &LIGHT_SAMPLING_PIPELINE,
    &EMISSIVE_BUILD_PIPELINE,
    &BOUNCE_ALLOC_PIPELINE,
    // 9.7 屏幕侧：空间/时域滤波（不受光线求交门控，恒建）
    &SPATIAL_FILTER_PIPELINE,
    &TEMPORAL_FILTER_PIPELINE,
    &SCREEN_GATHER_PIPELINE,
];

/// 宿主声明并建出的全部 set 布局。
pub(crate) const ALL_SETS: &[&SetLayoutDesc] = &[
    &BINDLESS_TEXTURE_SET_DESC,
    &FRAME_UNIFORM_SET_DESC,
    &INSTANCE_SET_DESC,
    &GBUFFER_SAMPLED_SET_DESC,
    &LIGHT_CLUSTER_SET_DESC,
    &TRANSPARENT_SET_DESC,
    &SHADOW_SET_DESC,
    &BLOOM_TEX_SET_DESC,
    &BLOOM_UP_SET_DESC,
    &SMAA_A_SET_DESC,
    &SMAA_B_SET_DESC,
    &HIZ_REDUCE_SET_DESC,
    &HIZ_OCCLUSION_SET_DESC,
    &IBL_CAPTURE_SET_DESC,
    &IBL_SH_PROJECT_SET_DESC,
    &IBL_SH_REDUCE_SET_DESC,
    &IBL_SH_CHECK_SET_DESC,
    &IBL_GLOBAL_PROBE_SET_DESC,
    &IBL_PREFILTER_SET_DESC,
    &IBL_DFG_SET_DESC,
    &IBL_SET_DESC,
    &RAY_TRACING_SET_DESC,
    &GI_SET_DESC,
    // 9.7.4 1/4 屏幕目标（两张图各双缓冲的四个 storage image 绑定）
    &SCGI_TARGET_SET_DESC,
];
