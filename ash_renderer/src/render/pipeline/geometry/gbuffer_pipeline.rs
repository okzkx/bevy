//! G-buffer 写入管线：不透明物体写 albedo / normal / matparam 三个颜色附件和深度。
//!
//! - set0/set1 布局来自 [`crate::render::BindlessTables`]，set2 来自
//!   [`crate::render::InstanceTables`]，不另建。没有 push：实例槽号走
//!   firstInstance，VS 用 `instance_index` 读。
//! - 顶点 32B 交错（位置 12 + 法线 12 + UV 8），与 mesh_convert 同一格式。
//! - reverse-Z：深度清 0、比较用 `GREATER`。
//! - 坑：viewport 用正高度。Y 已在 shader 里翻过，再用负高度翻一次画面会倒过来。
//! - 不剔除背面（有双面材质），不混合；viewport/scissor 动态设置，resize 不重建管线。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;

use crate::render::Context;

use crate::render::pipeline::base::pipeline_handle::{create_graphics_pipeline, PipelineHandle};

const GBUFFER_WRITE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gbuffer_write.spv"));

#[derive(bevy::prelude::Resource)]
pub struct GBufferPipeline {
    handle: PipelineHandle,
}

impl GBufferPipeline {
    /// 建管线链：shader module（build.rs 补丁产物）→ pipeline layout（三 set +
    /// push range）→ GraphicsPipelineCreateInfo（dynamic rendering 附件格式声明）。
    /// 任何失败回收已建对象（全有或全无）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        color_formats: &[vk::Format],
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &ctx.device;
        // 词流（小端）→ ash 的 &[u32]；build.rs 只写整词，as_chunks 即全部字节
        let words: Vec<u32> = GBUFFER_WRITE_SPV
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let module = unsafe {
            device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&words),
                None,
            )
        }?;
        let result = unsafe {
            Self::new_inner(
                device,
                color_formats,
                depth_format,
                set0_layout,
                set1_layout,
                set2_layout,
                module,
            )
        };
        if result.is_err() {
            unsafe { device.destroy_shader_module(module, None) };
        }
        result
    }

    /// 建链后半段（module 已建；失败路径回收 layout）。
    ///
    /// # Safety
    /// `module` 须刚创建成功且未销毁。
    unsafe fn new_inner(
        device: &Device,
        color_formats: &[vk::Format],
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
        set2_layout: vk::DescriptorSetLayout,
        module: vk::ShaderModule,
    ) -> Result<Self> {
        unsafe {
            // push range 已退役（5.3：firstInstance 携带实例槽号，见模块注释）
            let set_layouts = [set0_layout, set1_layout, set2_layout];
            let layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )?;
            let result =
                Self::create_pipeline(device, color_formats, depth_format, layout, module);
            if result.is_err() {
                device.destroy_pipeline_layout(layout, None);
            }
            result
        }
    }

    /// 管线状态装配（layout 已建；双入口名 vs_main/fs_main 与 WGSL 同源）。
    ///
    /// # Safety
    /// `module`/`layout` 须为合法句柄；入口名存在于 module。
    unsafe fn create_pipeline(
        device: &Device,
        color_formats: &[vk::Format],
        depth_format: vk::Format,
        layout: vk::PipelineLayout,
        module: vk::ShaderModule,
    ) -> Result<Self> {
        unsafe {
            let vs_main = c"vs_main";
            let fs_main = c"fs_main";
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(module)
                    .name(vs_main),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(module)
                    .name(fs_main),
            ];
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
            let blend_attachment: Vec<vk::PipelineColorBlendAttachmentState> = color_formats
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
                .color_attachment_formats(color_formats)
                .depth_attachment_format(depth_format);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
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
            device.destroy_shader_module(module, None);
            info!(
                "图形管线就绪: dynamic rendering（color {color_formats:?} / depth {depth_format:?}），\
                 顶点输入 32B 交错（pos/normal/uv @ 0/12/24），reverse-Z（clear 0 + GREATER），\
                 viewport 正高度直出，剔除关，push 退役（5.3：firstInstance 携带槽号），set0/set1/set2 同源绑定"
            );
            Ok(Self {
                handle: PipelineHandle::new(device, layout, pipeline),
            })
        }
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
