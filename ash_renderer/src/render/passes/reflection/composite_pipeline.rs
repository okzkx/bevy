//! 反射合成 pass（10.3.4）：把半分辨率反射按像素合成回 HDR 目标。
//!
//! 集合依次为帧 UBO、G-buffer 采样集（反照率/法线/深度/材质/采样器）、IBL 集（重算该像素
//! 原本的 IBL 高光，见计划 R5）、反射采样集（b0 反射图、b1 `hdr_copy`）。
//!
//! 为什么需要 IBL 集：合成 pass 早已把 IBL 高光乘曝光写进 HDR，SSR 后置只能「减掉它、
//! 换成命中色」才能不双重计环境光；减的那一项必须与合成端逐字节同式，所以走
//! `common/ibl.wgsl` 的 `ibl_specular_from`。

use crate::common::error::Result;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::record::graphics::PipelineRef;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::passes::ibl::IBL_SET_DESC;
use crate::render::passes::reflection::targets::REFLECTION_SAMPLE_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::Context;

/// 集合：帧 UBO、G-buffer 采样集、IBL 集、反射采样集。
pub(crate) static REFLECTION_COMPOSITE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "reflection_composite",
    shader: &shader_abi::REFLECTION_COMPOSITE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &IBL_SET_DESC,
        &REFLECTION_SAMPLE_SET_DESC,
    ],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

/// 反射合成管线（配方 [`REFLECTION_COMPOSITE_PIPELINE`]）。
#[derive(bevy::prelude::Resource)]
pub struct ReflectionCompositePipeline {
    inner: FullscreenPipeline,
}

impl ReflectionCompositePipeline {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            inner: FullscreenPipeline::new(ctx, &REFLECTION_COMPOSITE_PIPELINE, layouts)?,
        })
    }

    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.inner.rec()
    }
}
