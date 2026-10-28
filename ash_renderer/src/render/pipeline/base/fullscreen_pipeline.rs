//! 全屏管线骨架：没有顶点输入（VS 用 `vertex_index` 生成盖满视口的三角形）、
//! 没有深度、不混合、单颜色附件、动态 viewport/scissor。合成、tonemap、Bloom、
//! SMAA 各自传入 shader 和 set 布局复用它；这里还有录全屏 pass 的助手。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::Context;

use crate::render::core::barrier::{image_barrier, ImageBarrier};
use crate::render::pipeline::base::pipeline_handle::{
    bind_graphics, create_graphics_pipeline, full_rect, set_viewport_scissor, PipelineHandle,
};

/// 全屏管线 + pipeline layout（dynamic rendering 形态）。
#[derive(bevy::prelude::Resource)]
pub struct FullscreenPipeline {
    handle: PipelineHandle,
}

impl FullscreenPipeline {
    /// 用调用方给的 shader 和 set 布局建管线，没有 push。set 布局里这个 pass
    /// 用不到的 binding 可以闲置，只要描述符都写过。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        spv: &'static [u8],
        color_format: vk::Format,
        set_layouts: &[vk::DescriptorSetLayout],
    ) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = spv
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
        }?;
        let result = unsafe { Self::new_inner(device, color_format, set_layouts, module) };
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
        color_format: vk::Format,
        set_layouts: &[vk::DescriptorSetLayout],
        module: vk::ShaderModule,
    ) -> Result<Self> {
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(set_layouts),
                    None,
                )?;
            let result = Self::create_pipeline(device, color_format, layout, module);
            if result.is_err() {
                device.destroy_pipeline_layout(layout, None);
            }
            result
        }
    }

    /// 管线状态装配（双入口名 vs_main/fs_main 与 WGSL 同源）。
    ///
    /// # Safety
    /// `module`/`layout` 须为合法句柄；入口名存在于 module。
    unsafe fn create_pipeline(
        device: &Device,
        color_format: vk::Format,
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
            device.destroy_shader_module(module, None);
            info!(
                "全屏管线就绪: color {color_format:?}（无顶点输入全屏三角形 / 无深度 / 无混合，动态 viewport）"
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

    /// pipeline layout 句柄（描述符绑定与管线同源）。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.handle.layout()
    }
}

/// 全屏三角形 pass 的参数：一个颜色附件（写完保留，无深度）、管线、从 set 0 起绑定的描述符集。
pub(crate) struct FullscreenPass<'a> {
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    /// `Some` 先清成这个颜色；`None` 不清也不保留旧内容，前提是这个 pass 会写满每个像素。
    pub clear: Option<[f32; 4]>,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub sets: &'a [vk::DescriptorSet],
}

/// 录一个完整的全屏 pass（[`record_fullscreen_open`] 之后结束渲染）。
///
/// # Safety
/// `cb` 处于录制状态，句柄合法且活到命令执行完；`pass.view` 须已处于 COLOR_ATTACHMENT_OPTIMAL 布局。
pub(crate) unsafe fn record_fullscreen(device: &Device, cb: vk::CommandBuffer, pass: &FullscreenPass<'_>) {
    unsafe {
        record_fullscreen_open(device, cb, pass);
        device.cmd_end_rendering(cb);
    }
}

/// 开始渲染、绑定、设 viewport/scissor、画全屏三角形，但**不结束**渲染：
/// 调用方可以在同一次渲染里接着画别的，最后自己调 `cmd_end_rendering`。
///
/// # Safety
/// 同 [`record_fullscreen`]。
pub(crate) unsafe fn record_fullscreen_open(
    device: &Device,
    cb: vk::CommandBuffer,
    pass: &FullscreenPass<'_>,
) {
    let attachment = vk::RenderingAttachmentInfo::default()
        .image_view(pass.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .store_op(vk::AttachmentStoreOp::STORE);
    let attachment = match pass.clear {
        Some(float32) => attachment
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32 } }),
        None => attachment.load_op(vk::AttachmentLoadOp::DONT_CARE),
    };
    unsafe {
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(full_rect(pass.extent))
                .layer_count(1)
                .color_attachments(slice::from_ref(&attachment)),
        );
        bind_graphics(device, cb, pass.pipeline, pass.layout, pass.sets);
        set_viewport_scissor(device, cb, pass.extent);
        // 3 个顶点、无顶点 buffer：VS 按 vertex_index 生成一个盖满视口的三角形
        device.cmd_draw(cb, 3, 1, 0, 1);
    }
}

/// 中间目标"先写后采样"的一整轮：屏障丢弃旧内容进附件（等 `entry_src_stage`）→ 全屏 pass → 屏障转可采样。
/// 内容每轮都重写，所以不管 `image` 上一轮停在什么布局，从 UNDEFINED 进都合法。
///
/// # Safety
/// 同 [`record_fullscreen`]；`image` 是 `pass.view` 所属的图像。
pub(crate) unsafe fn record_fullscreen_sampled(
    device: &Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    entry_src_stage: vk::PipelineStageFlags,
    pass: &FullscreenPass<'_>,
) {
    unsafe {
        image_barrier(device, cb, &ImageBarrier::discard_to_color(image, entry_src_stage));
        record_fullscreen(device, cb, pass);
        image_barrier(device, cb, &ImageBarrier::color_to_sampled(image));
    }
}
