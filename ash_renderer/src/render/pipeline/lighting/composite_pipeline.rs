//! 延迟光照合成：读 G-buffer、光源分簇和阴影，把不透明物体的光照算进 HDR 目标。

use ash::vk;

use crate::common::error::Result;
use crate::render::pipeline::base::fullscreen_pipeline::FullscreenPipeline;
use crate::render::Context;

const GBUFFER_COMPOSITE_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gbuffer_composite.spv"));

/// 合成管线。set 依次为帧 UBO、G-buffer 采样集、光源分簇集、阴影集；这些布局
/// 归各自资源，本管线要先于它们销毁。
#[derive(bevy::prelude::Resource)]
pub struct CompositePipeline {
    inner: FullscreenPipeline,
}

impl CompositePipeline {
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        hdr_format: vk::Format,
        frame_layout: vk::DescriptorSetLayout,
        gbuffer_layout: vk::DescriptorSetLayout,
        cluster_layout: vk::DescriptorSetLayout,
        shadow_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        Ok(Self {
            inner: FullscreenPipeline::new(
                ctx,
                GBUFFER_COMPOSITE_SPV,
                hdr_format,
                &[frame_layout, gbuffer_layout, cluster_layout, shadow_layout],
            )?,
        })
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
