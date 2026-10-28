//! present tonemap：HDR 加 Bloom，经 ACES 映射到 LDR 目标，再交给 SMAA。

use ash::vk;

use crate::common::error::Result;
use crate::render::core::fullscreen_pipeline::FullscreenPipeline;
use crate::render::Context;

const PRESENT_TONEMAP_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/present_tonemap.spv"));

/// tonemap 管线。set 依次为帧 UBO（调试模式旁路用）、G-buffer 采样集（取 HDR）、
/// Bloom 升采样集（取 Bloom 结果和参数）。
#[derive(bevy::prelude::Resource)]
pub struct PresentTonemapPipeline {
    inner: FullscreenPipeline,
}

impl PresentTonemapPipeline {
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        color_format: vk::Format,
        frame_layout: vk::DescriptorSetLayout,
        gbuffer_layout: vk::DescriptorSetLayout,
        bloom_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        Ok(Self {
            inner: FullscreenPipeline::new(
                ctx,
                PRESENT_TONEMAP_SPV,
                color_format,
                &[frame_layout, gbuffer_layout, bloom_layout],
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
