//! 延迟光照合成：读 G-buffer、光源分簇和阴影，把不透明物体的光照算进 HDR 目标。

use ash::vk;

use crate::common::error::Result;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::pipeline::graphics::PipelineRef;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET;
use crate::render::passes::lighting::cluster::LIGHT_CLUSTER_SET;
use crate::render::passes::shadow::shadow_map::SHADOW_SET;
use crate::render::resident::descriptors::FRAME_SET;
use crate::render::Context;

/// set 依次为帧 UBO、G-buffer 采样集、光源分簇集、阴影集。
pub(crate) static GBUFFER_COMPOSITE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gbuffer_composite",
    shader: &shader_abi::GBUFFER_COMPOSITE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&FRAME_SET, &GBUFFER_SAMPLED_SET, &LIGHT_CLUSTER_SET, &SHADOW_SET],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

/// 合成管线（配方 [`GBUFFER_COMPOSITE_PIPELINE`]）；用到的布局归各自资源，本管线要
/// 先于它们销毁。
#[derive(bevy::prelude::Resource)]
pub struct CompositePipeline {
    inner: FullscreenPipeline,
}

impl CompositePipeline {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self { inner: FullscreenPipeline::new(ctx, &GBUFFER_COMPOSITE_PIPELINE, layouts)? })
    }

    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.inner.rec()
    }

    pub fn pipeline(&self) -> vk::Pipeline {
        self.inner.pipeline()
    }

    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.inner.layout()
    }
}
