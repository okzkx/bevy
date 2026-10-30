//! 布局契约登记表：宿主建的全部管线配方与 set 声明的全量清单（O2）。新增管线/set
//! 必须登记在这里，否则 [`every_shader_entry_has_a_pipeline`]（契约测试）和
//! `ash_renderer/layout_contract`（BRP 报表）会报。
//!
//! 这个模块刻意不挂测试：BRP 只读方法在运行期也要读它。

use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayoutDesc;
use crate::render::core::binding::shader_abi::{self, Shader};
use crate::render::passes::gbuffer::gbuffer_pipeline::GBUFFER_WRITE_PIPELINE;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET;
use crate::render::passes::lighting::cluster::{CLUSTER_BUILD_PIPELINE, LIGHT_CLUSTER_SET};
use crate::render::passes::lighting::composite_pipeline::GBUFFER_COMPOSITE_PIPELINE;
use crate::render::passes::overlay::overlay_pipeline::OVERLAY_PIPELINE;
use crate::render::passes::post::bloom::{
    BLOOM_DOWN_PIPELINE, BLOOM_PREFILTER_PIPELINE, BLOOM_TEX_SET, BLOOM_UP_PIPELINE, BLOOM_UP_SET,
};
use crate::render::passes::post::present_tonemap_pipeline::PRESENT_TONEMAP_PIPELINE;
use crate::render::passes::post::smaa::{
    SMAA_A_SET, SMAA_BLEND_PIPELINE, SMAA_B_SET, SMAA_EDGE_PIPELINE, SMAA_NEIGHBORHOOD_PIPELINE,
};
use crate::render::passes::shadow::shadow_build::SHADOW_BUILD_PIPELINE;
use crate::render::passes::shadow::shadow_map::{SHADOW_SET, SHADOW_WRITE_PIPELINE};
use crate::render::passes::transparent::{
    TRANSPARENT_COLLECT_PIPELINE, TRANSPARENT_FORWARD_PIPELINE, TRANSPARENT_GATHER_PIPELINE,
    TRANSPARENT_SET, TRANSPARENT_SORT_PIPELINE,
};
use crate::render::passes::visibility::cull::CULL_PIPELINE;
use crate::render::passes::visibility::hiz::{HIZ_DOWNSAMPLE_PIPELINE, HIZ_OCCLUSION_SET, HIZ_REDUCE_SET};
use crate::render::passes::visibility::indirect::BUILD_INDIRECT_PIPELINE;
use crate::render::passes::visibility::lod::LOD_SELECT_PIPELINE;
use crate::render::passes::visibility::occlusion::OCCLUSION_PIPELINE;
use crate::render::resident::descriptors::BINDLESS_SET;
use crate::render::resident::frame_uniforms::FRAME_SET;
use crate::render::resident::instance_table::INSTANCE_SET;

/// 宿主建的全部管线。
pub(crate) const ALL_PIPELINES: &[&PipelineDesc] = &[
    &GBUFFER_WRITE_PIPELINE,
    &SHADOW_WRITE_PIPELINE,
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
];

/// 宿主声明并建出的全部 set 布局。
pub(crate) const ALL_SETS: &[&SetLayoutDesc] = &[
    &BINDLESS_SET,
    &FRAME_SET,
    &INSTANCE_SET,
    &GBUFFER_SAMPLED_SET,
    &LIGHT_CLUSTER_SET,
    &TRANSPARENT_SET,
    &SHADOW_SET,
    &BLOOM_TEX_SET,
    &BLOOM_UP_SET,
    &SMAA_A_SET,
    &SMAA_B_SET,
    &HIZ_REDUCE_SET,
    &HIZ_OCCLUSION_SET,
];

/// 只给 examples 用、不归宿主管线的 shader（仅契约测试引用）。
#[cfg_attr(not(test), expect(dead_code, reason = "只有契约测试区分探针 shader"))]
pub(crate) const PROBE_ONLY: &[&Shader] = &[&shader_abi::DESCRIPTOR_PROBE];
