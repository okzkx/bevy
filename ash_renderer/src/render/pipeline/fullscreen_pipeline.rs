//! 全屏绘制管线（8.1）：合成（G-buffer 光照）与 present blit 共用的管线骨架
//! ——无顶点输入（VS 按 `@builtin(vertex_index)` 直出全屏三角形，无顶点
//! buffer）、无深度附件、无混合、单 color 附件、动态 viewport/scissor。
//!
//! set 布局由调用方传入（集与布局同源纪律）：合成 pass = [set1 帧UBO layout,
//! G-buffer 采样集 layout, cluster 集 layout, shadow 域 layout（8.3）]，
//! present blit = [G-buffer 采样集 layout]——
//! 采样集布局里本 pass 用不到的 binding 合法闲置（描述符写入完整，非
//! partially-bound 的悬空）。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::Context;

use crate::render::core::barrier::{image_barrier, ImageBarrier};
use super::pipeline_handle::{
    bind_graphics, create_graphics_pipeline, full_rect, set_viewport_scissor, PipelineHandle,
};

/// shader 模块字节（build.rs 产物，OUT_DIR 之下，与主管线同一编译路径）。
const GBUFFER_COMPOSITE_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gbuffer_composite.spv"));
const PRESENT_TONEMAP_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/present_tonemap.spv"));

/// 全屏管线 + pipeline layout（dynamic rendering 形态）。
#[derive(bevy::prelude::Resource)]
pub struct FullscreenPipeline {
    handle: PipelineHandle,
}

impl FullscreenPipeline {
    /// 建管线链：shader module（build.rs 产物，spirv-val 已过）→ layout
    ///（调用方给的 set layouts，无 push）→ GraphicsPipelineCreateInfo。
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

/// 合成管线资源（8.1/8.2.3）：G-buffer 光照累积 → HDR 中间目标。
/// layout = [set1 帧 UBO, G-buffer 采样集, cluster 集]（集与布局同源纪律——
/// 三个 layout 对象分别借用 BindlessTables、GBufferTargets 与 ClusterTargets，
/// 拆除序先于三者）。cluster 集为 8.2.3 光照累积的取数面（b0 网格参数 /
/// b1 光源常驻表 / b2 逐 cluster 列表）。
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

    /// 管线句柄。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.inner.pipeline()
    }

    /// layout 句柄。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.inner.layout()
    }
}

/// present tonemap 管线资源（8.4 接缝正式启用；8.1 的 blit 退役）：HDR + Bloom
/// → ACES → swapchain。layout = [帧 UBO, G-buffer 采样集, bloom 参数集]——
/// mode 旁路面（debug 仪器直出）+ hdr + bloom/参数。
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

    /// 管线句柄。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.inner.pipeline()
    }

    /// layout 句柄。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.inner.layout()
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
