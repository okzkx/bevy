//! present tonemap：HDR 加 Bloom，经 ACES 映射到 LDR 目标，再交给 SMAA。

use ash::vk;

use crate::common::error::Result;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET;
use crate::render::passes::post::bloom::BLOOM_UP_SET;
use crate::render::resident::descriptors::FRAME_SET;
use crate::render::Context;

/// set 依次为帧 UBO（调试模式旁路用）、G-buffer 采样集（取 HDR）、Bloom 升采样集
///（取 Bloom 结果和参数）。
pub(crate) static PRESENT_TONEMAP_PIPELINE: PipelineDesc = PipelineDesc {
    label: "present_tonemap",
    shader: &shader_abi::PRESENT_TONEMAP,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&FRAME_SET, &GBUFFER_SAMPLED_SET, &BLOOM_UP_SET],
    push: None,
};

/// tonemap 管线（配方 [`PRESENT_TONEMAP_PIPELINE`]）。
#[derive(bevy::prelude::Resource)]
pub struct PresentTonemapPipeline {
    inner: FullscreenPipeline,
}

impl PresentTonemapPipeline {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts, color_format: vk::Format) -> Result<Self> {
        Ok(Self { inner: FullscreenPipeline::new(ctx, &PRESENT_TONEMAP_PIPELINE, layouts, color_format)? })
    }

    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.inner.pipeline()
    }

    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.inner.layout()
    }
}
