//! 透明前向图形管线：alpha 混合画进 HDR 目标，深度只测不写。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::Context;
use crate::render::core::pipeline::graphics::{create_graphics_pipeline, PipelineHandle};
use crate::render::core::record::graphics::PipelineRef;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::passes::lighting::cluster::LIGHT_CLUSTER_SET_DESC;
use crate::render::resident::tables::bindless_texture_table::BINDLESS_TEXTURE_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;

/// set 依次为 set0/set1/set2 和光源分簇集。
pub(crate) static TRANSPARENT_FORWARD_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_forward",
    shader: &shader_abi::TRANSPARENT_FORWARD,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BINDLESS_TEXTURE_SET_DESC, &FRAME_UNIFORM_SET_DESC, &INSTANCE_SET_DESC, &LIGHT_CLUSTER_SET_DESC],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: Some(crate::render::core::target::depth::DEPTH_FORMAT),
};

/// 透明前向图形管线（配方 [`TRANSPARENT_FORWARD_PIPELINE`]）。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPipeline {
    handle: PipelineHandle,
}

impl TransparentPipeline {
    /// 坑：声明的颜色/深度格式必须与实际绑定的附件一致，否则报 VUID-08914。
    ///
    /// # Errors
    /// 配方对账不过，或 layout / shader module / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        TRANSPARENT_FORWARD_PIPELINE.build(ctx, layouts, |layout, stages| unsafe {
            Self::create_pipeline(&ctx.device, layout, stages)
        })
    }

    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点格式与场景管线一致：位置 12B + 法线 12B + UV 8B，共 32B
        let vertex_binding = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(crate::render::VERTEX_STRIDE as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(12),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(24),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_binding)
            .vertex_attribute_descriptions(&vertex_attributes);
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
        // reverse-Z 用 GREATER；透明不写深度
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::GREATER)
            .min_depth_bounds(0.0)
            .max_depth_bounds(1.0);
        // 非预乘 alpha 混合：src·α + dst·(1−α)；alpha 通道直接写 src
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(TRANSPARENT_FORWARD_PIPELINE.color_formats)
            .depth_attachment_format(TRANSPARENT_FORWARD_PIPELINE.depth_format.unwrap_or(vk::Format::UNDEFINED));
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
            "透明前向管线就绪: HDR {:?}（SRC_ALPHA 混合）+ 深度 {:?}（重绑只测不写），常驻三集 + 光源分簇集",
            TRANSPARENT_FORWARD_PIPELINE.color_formats,
            TRANSPARENT_FORWARD_PIPELINE.depth_format
        );
        Ok(Self {
            handle: PipelineHandle::new(device, layout, pipeline, TRANSPARENT_FORWARD_PIPELINE.sets),
        })
    }

    /// 录制投影（帧输入束携带）。
    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.handle.rec()
    }
}
