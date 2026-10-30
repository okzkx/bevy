//! G-buffer 写入管线：不透明物体写 albedo / normal / matparam 三个颜色附件和深度。
//!
//! - 布局配方 [`GBUFFER_WRITE_PIPELINE`]：set0 是 [`crate::render::BindlessTextureTable`] 的常驻集，
//!   set1 是 [`crate::render::FrameUniformTable`] 的每帧集，set2 是 [`crate::render::InstanceTables`] 的实例表，不另建。没有 push：
//!   实例槽号走 firstInstance，VS 用 `instance_index` 读。
//! - 顶点 32B 交错（位置 12 + 法线 12 + UV 8），与 mesh_convert 同一格式。
//! - reverse-Z：深度清 0、比较用 `GREATER`。
//! - 坑：viewport 用正高度。Y 已在 shader 里翻过，再用负高度翻一次画面会倒过来。
//! - 不剔除背面（有双面材质），不混合；viewport/scissor 动态设置，resize 不重建管线。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;

use crate::render::Context;

use crate::render::core::pipeline::graphics::{PipelineRef, create_graphics_pipeline, PipelineHandle};
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::binding::shader_abi;
use crate::render::resident::bindless_texture_table::BINDLESS_TEXTURE_SET_DESC;
use crate::render::resident::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::resident::instance_table::INSTANCE_SET_DESC;

pub(crate) static GBUFFER_WRITE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "gbuffer_write",
    shader: &shader_abi::GBUFFER_WRITE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BINDLESS_TEXTURE_SET_DESC, &FRAME_UNIFORM_SET_DESC, &INSTANCE_SET_DESC],
    push: None,
    color_formats: &[crate::render::ALBEDO_FORMAT, crate::render::NORMAL_FORMAT, crate::render::MATPARAM_FORMAT],
    depth_format: Some(crate::render::core::target::depth::DEPTH_FORMAT),
};

#[derive(bevy::prelude::Resource)]
pub struct GBufferPipeline {
    handle: PipelineHandle,
}

impl GBufferPipeline {
    /// 按 [`GBUFFER_WRITE_PIPELINE`] 建管线（骨架见 `PipelineDesc::build`），附件格式走
    /// dynamic rendering 声明。任何失败回收已建对象（全有或全无）。
    ///
    /// # Errors
    /// 配方对账不过，或 layout / shader module / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        layouts: &SetLayouts,
    ) -> Result<Self> {
        GBUFFER_WRITE_PIPELINE.build(ctx, layouts, |layout, stages| unsafe {
            Self::create_pipeline(&ctx.device, &GBUFFER_WRITE_PIPELINE, layout, stages)
        })
    }

    /// 管线状态装配（layout 已建；入口取自配方）。
    ///
    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        desc: &'static PipelineDesc,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点输入 = mesh_convert 的同一份 32B 交错契约（三属性偏移 0/12/24）
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
        // viewport/scissor 动态：尺寸随 swapchain 逐帧给（正高度直出，朝向定案见模块注释）
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            // 关剔除：HoseMat 双面 + 镜片按不透明覆盖；正高度直出未镜像
            // 绕向，开剔除前的绕向核验另立判定（面板定案）
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // reverse-Z 成套：清 0 + GREATER（bevy 无限反向透视）；stencil 全关
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::GREATER)
            .min_depth_bounds(0.0)
            .max_depth_bounds(1.0);
        // 逐 color 附件一条 blend 态（8.1 起 G-buffer pass 双 color；
        // 声明面 = 实例附件全集，VUID-08914 纪律）
        let blend_attachment: Vec<vk::PipelineColorBlendAttachmentState> = desc.color_formats
            .iter()
            .map(|_| {
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(false)
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
            })
            .collect();
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        // dynamic rendering：附件格式内联声明（无 RenderPass/Framebuffer）
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(desc.color_formats)
            .depth_attachment_format(desc.depth_format.expect("gbuffer_write 声明深度"));
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
        // module 用毕即拆：管线创建完成即编译收口，规范允许此刻销毁 shader
        // module（vkDestroyShaderModule 与管线无生命周期耦合）。句柄是纯整数，
        // 不拆即每次启动泄漏 1 个 VkShaderModule（验证层
        // VUID-vkDestroyDevice-device-05137 可抓获此类泄漏）。
        info!(
            "图形管线就绪: dynamic rendering（color {:?} / depth {:?}），\
             顶点输入 32B 交错（pos/normal/uv @ 0/12/24），reverse-Z（clear 0 + GREATER），\
             viewport 正高度直出，剔除关，push 退役（5.3：firstInstance 携带槽号），set0/set1/set2 同源绑定",
            desc.color_formats,
            desc.depth_format
        );
        Ok(Self {
            handle: PipelineHandle::new(device, layout, pipeline, GBUFFER_WRITE_PIPELINE.sets),
        })
    }

    /// 录制投影（帧输入束携带）。
    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.handle.rec()
    }

    /// 管线句柄（帧录制绑定用）。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.handle.pipeline()
    }

    /// pipeline layout 句柄（push/描述符绑定与管线同源）。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.handle.layout()
    }
}
