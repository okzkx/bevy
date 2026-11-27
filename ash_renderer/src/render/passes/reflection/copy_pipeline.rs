//! 反射段的 HDR 拷贝通道（10.3.2）：把合成写完的 HDR 抄进本帧槽的 `hdr_copy`。
//!
//! 只有 G-buffer 采样集一个集（b2 = HDR、b4 = 采样器），没有帧 UBO 也没有 push——
//! 全屏管线本来就没有 push（见 `core/pipeline/fullscreen.rs`）。
//!
//! 关档时不录这一段（`RenderFeatures::reflection` 为 `None`）：既然步进与合成都不跑，
//! 拷贝也没有意义，省掉一次全屏写。

use crate::common::error::Result;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::record::graphics::PipelineRef;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::Context;

/// 集合：G-buffer 采样集（b2 = HDR 中间目标、b4 = 采样器）。
pub(crate) static REFLECTION_COPY_PIPELINE: PipelineDesc = PipelineDesc {
    label: "reflection_copy",
    shader: &shader_abi::REFLECTION_COPY,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&GBUFFER_SAMPLED_SET_DESC],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

/// HDR 拷贝管线（配方 [`REFLECTION_COPY_PIPELINE`]）。
#[derive(bevy::prelude::Resource)]
pub struct ReflectionCopyPipeline {
    inner: FullscreenPipeline,
}

impl ReflectionCopyPipeline {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            inner: FullscreenPipeline::new(ctx, &REFLECTION_COPY_PIPELINE, layouts)?,
        })
    }

    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.inner.rec()
    }
}
