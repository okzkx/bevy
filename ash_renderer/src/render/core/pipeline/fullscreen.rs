//! 全屏管线骨架：没有顶点输入（VS 用 `vertex_index` 生成盖满视口的三角形）、
//! 没有深度、不混合、单颜色附件、动态 viewport/scissor。合成、tonemap、Bloom、
//! SMAA 各自传入布局配方复用它；录全屏 pass 见 `record::fullscreen`。

use crate::render::core::pipeline::graphics::PipelineRef;
use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::Context;

use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::pipeline::graphics::{create_graphics_pipeline, PipelineHandle};

/// 全屏管线 + pipeline layout（dynamic rendering 形态）。
#[derive(bevy::prelude::Resource)]
pub struct FullscreenPipeline {
    handle: PipelineHandle,
}

impl FullscreenPipeline {
    /// 按配方建管线（配方对账见 `pipeline_layout`，全屏管线没有 push）。set 布局里
    /// 这个 pass 用不到的 binding 可以闲置，只要描述符都写过。
    ///
    /// # Errors
    /// 配方对账不过，或 layout / shader module / pipeline 任一创建失败。
    pub(crate) fn new(
        ctx: &Context,
        desc: &PipelineDesc,
        layouts: &SetLayouts,
        color_format: vk::Format,
    ) -> Result<Self> {
        desc.build(ctx, layouts, |layout, stages| unsafe {
            Self::create_pipeline(&ctx.device, desc, color_format, layout, stages)
        })
    }

    /// 管线状态装配。
    ///
    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        desc: &PipelineDesc,
        color_format: vk::Format,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 无顶点输入：pVertexInputState 不能为 NULL（VUID-pStages-02097），
        // 空绑定/空属性的全顶点输入状态即"无顶点"的合法表达
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // 无深度附件：深度测试/写全关（VUID-08914 侧：本管线的
        // depthAttachmentFormat 声明走 UNDEFINED = 不声明深度）
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default();
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [color_format];
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_info)
            .layout(layout)
            .push_next(&mut rendering_info);
        let pipeline = create_graphics_pipeline(device, &info)?;
        info!(
            "全屏管线 {} 就绪: color {color_format:?}（无顶点输入全屏三角形 / 无深度 / 无混合，动态 viewport）",
            desc.label
        );
        Ok(Self {
            handle: PipelineHandle::new(device, layout, pipeline, desc.sets),
        })
    }

    /// 管线句柄（帧录制绑定用）。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.handle.pipeline()
    }

    /// pipeline layout 句柄（描述符绑定与管线同源）。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.handle.layout()
    }

    /// 录制投影（帧输入束携带）。
    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.handle.rec()
    }
}
