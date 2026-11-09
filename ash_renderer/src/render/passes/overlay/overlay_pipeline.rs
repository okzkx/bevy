//! overlay 图形管线：画 egui 镶嵌出的顶点。
//!
//! - **set0 与场景管线共用**：layout 只声明常驻表 set0（UI 无 UBO）。切换管线后 set0
//!   要按本管线的 layout 重绑（同一个集，不同 layout 对象）。
//! - **push 16B**：screen_size @0 + tex_index @8 + sampler_index @12，整帧 UI 一次；
//!   screen_size 用 points（与顶点 pos 同域，tessellate 不按 ppp 缩放顶点）。
//!   `overlay_draw.wgsl` 的 PushParams、[`UiPushLayout`] 的 `offset_of!` 断言与本配方
//!   三处要一起改。
//! - **深度格式照声明、读写全关**：坑：渲染实例带 D32 深度附件，未启用
//!   dynamicRenderingUnusedAttachments 时管线的 depthAttachmentFormat 必须与之相等，
//!   不能填 UNDEFINED（VUID-08914，验证层逐 draw 报错）；"不用深度"靠关测试/写入表达。
//! - **混合**：color = (ONE, ONE_MINUS_SRC_ALPHA)，alpha = (ONE_MINUS_DST_ALPHA, ONE)——
//!   egui 顶点色是预乘的，src 因子取 ONE（与 egui 官方 wgpu 后端一致）。
//! - **不剔除**：epaint 不保证 UI 顶点绕向一致。
//!
//! UI 录制契约 [`UiPaint`]/[`UiDrawCall`] 也住本模块：录制层 `render::frame::record`
//! 与生产方 `overlay::framework::paint` 都用它，render 不引上层类型。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::{put_f32s, put_u32};
use crate::common::error::Result;

use crate::render::Context;

use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{stages, DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::graphics::{create_graphics_pipeline, PipelineHandle};
use crate::render::core::record::graphics::{
    bind_graphics, bind_vertex_index, set_viewport, PipelineRef,
};
use crate::render::resident::tables::bindless_texture_table::BINDLESS_TEXTURE_SET_DESC;

/// push 范围覆盖全 16B、双阶段：screen_size 在 vertex 用，槽位在 fragment 用。
pub(crate) static OVERLAY_PIPELINE: PipelineDesc = PipelineDesc {
    label: "overlay_draw",
    shader: &shader_abi::OVERLAY_DRAW,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BINDLESS_TEXTURE_SET_DESC],
    push: Some(PushRange {
        stages: stages(&[vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]),
        size: UI_PUSH_CONSTANTS_SIZE,
    }),
    color_formats: &[crate::render::SWAPCHAIN_COLOR_FORMAT],
    depth_format: Some(crate::render::core::target::depth::DEPTH_FORMAT),
};

/// push constant 布局（16B）的 Rust 镜像：运行时不用（字节由 [`pack_ui_push`] 按偏移
/// 手工排），只为让 `offset_of!` 静态断言钉住偏移。
#[repr(C)]
struct UiPushLayout {
    screen_size: [f32; 2],
    tex_index: u32,
    sampler_index: u32,
}

const _: () = {
    assert!(std::mem::offset_of!(UiPushLayout, screen_size) == 0);
    assert!(std::mem::offset_of!(UiPushLayout, tex_index) == 8);
    assert!(std::mem::offset_of!(UiPushLayout, sampler_index) == 12);
    assert!(std::mem::size_of::<UiPushLayout>() == 16);
};

/// overlay push constant 总长。
pub(crate) const UI_PUSH_CONSTANTS_SIZE: u32 = 16;

/// UI 顶点步长 20B（[`super::ui_vertex_ring::UiVertex`] 的 pos 8 + uv 8 + color 4；
/// 那边有 `size_of` 断言对齐本常量）。
pub(crate) const UI_VERTEX_STRIDE: u32 = 20;

/// overlay push 参数 → 16B 字节（偏移同 [`UiPushLayout`]）。
#[must_use]
pub(crate) fn pack_ui_push(
    screen_size: [f32; 2],
    tex_index: u32,
    sampler_index: u32,
) -> [u8; UI_PUSH_CONSTANTS_SIZE as usize] {
    let mut out = [0u8; UI_PUSH_CONSTANTS_SIZE as usize];
    put_f32s(&mut out, 0, &screen_size);
    put_u32(&mut out, 8, tex_index);
    put_u32(&mut out, 12, sampler_index);
    out
}

/// 一个 UI clip 的 draw 参数：UI 环内的索引/顶点定位 + scissor（物理像素）。
#[derive(Clone, Copy, Debug)]
pub struct UiDrawCall {
    pub index_count: u32,
    pub first_index: u32,
    pub vertex_base: u32,
    /// (x, y, w, h) 物理像素，左上原点——Vulkan scissor 同域。
    pub scissor: [u32; 4],
}

/// 一次 UI 绘制的全部产物（`overlay::framework::paint` 组装，`render::frame::record` 消费）。
/// 顶点/索引已由生产方写进 UI 环本帧槽的 buffer。
pub struct UiPaint {
    /// 图集纹理槽（push 的 tex_index）。
    pub atlas_texture: u32,
    /// 图集采样器槽（push 的 sampler_index）。
    pub atlas_sampler: u32,
    /// 屏幕尺寸，单位 points（push 的 screen_size；已除过 ppp）。
    pub screen_pt: [f32; 2],
    /// 每个 clip 一条 draw，各带自己的 scissor。
    pub draws: Vec<UiDrawCall>,
}

impl UiPaint {
    /// 录制 UI 绘制，接在 neighborhood 的同一次渲染里（渲染实例由调用方开，按光栅序
    /// 盖在场景之上）。顶点/索引在本帧槽的 UI 环 buffer（`buffers`）里。
    pub fn record(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        overlay: PipelineRef,
        bindless_set: DescSet,
        extent: vk::Extent2D,
        buffers: (vk::Buffer, vk::Buffer),
    ) {
        unsafe {
            // 描述符绑定跟随 pipeline layout：切到 overlay 管线后 set0 要按它的 layout 重绑
            bind_graphics(device, cb, &overlay, &[bindless_set]);
            // 场景分支不一定跑过，UI 自己设 viewport
            set_viewport(device, cb, extent);
            bind_vertex_index(device, cb, buffers.0, buffers.1);
            // push 常量整帧不变，循环外打包一次
            let ui_push = pack_ui_push(self.screen_pt, self.atlas_texture, self.atlas_sampler);
            for call in &self.draws {
                // 每个 clip 设一次 scissor（零尺寸 = 全裁）
                device.cmd_set_scissor(
                    cb,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D {
                            x: call.scissor[0] as i32,
                            y: call.scissor[1] as i32,
                        },
                        extent: vk::Extent2D {
                            width: call.scissor[2],
                            height: call.scissor[3],
                        },
                    }],
                );
                device.cmd_push_constants(
                    cb,
                    overlay.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &ui_push,
                );
                device.cmd_draw_indexed(
                    cb,
                    call.index_count,
                    1,
                    call.first_index,
                    call.vertex_base as i32,
                    0,
                );
            }
        }
    }
}

/// ⑧ UI 绘制束：overlay 管线 + 本帧 UI 绘制产物 + 顶点/索引环 buffer（同生同灭，
/// 无 UI 整段跳过）。
pub struct UiDraw<'a> {
    /// overlay 管线。自有 layout（只有 set0），切到它之后 set0 要重绑。
    pub overlay: PipelineRef,
    /// 本帧 UI 绘制产物。
    pub paint: &'a UiPaint,
    /// 本帧槽 UI 顶点/索引 buffer（本帧已写入）。
    pub buffers: (vk::Buffer, vk::Buffer),
}

impl UiDraw<'_> {
    /// 录制 UI 绘制，接在 neighborhood 的同一次渲染里（渲染实例由调用方开，按光栅序
    /// 盖在场景之上）。描述符绑定跟随 overlay 的 pipeline layout，set0 重绑在
    /// [`UiPaint::record`] 内。
    pub fn record(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        bindless_set: DescSet,
        extent: vk::Extent2D,
    ) {
        self.paint
            .record(device, cb, self.overlay, bindless_set, extent, self.buffers);
    }
}

/// overlay 图形管线 + pipeline layout（只声明 set0 + 16B push）。
#[derive(bevy::prelude::Resource)]
pub struct OverlayPipeline {
    handle: PipelineHandle,
}

impl OverlayPipeline {
    /// 按 [`OVERLAY_PIPELINE`] 建管线（骨架见 `PipelineDesc::build`）。任何失败回收已建
    /// 对象（全有或全无）。
    ///
    /// # Errors
    /// 配方对账不过，或 layout / shader module / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        OVERLAY_PIPELINE.build(ctx, layouts, |layout, stages| unsafe {
            Self::create_pipeline(&ctx.device, layout, stages)
        })
    }

    /// 管线状态装配（layout 已建；入口取自配方）。
    ///
    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点输入：20B 交错（pos/uv/color @ 0/8/16）
        let vertex_binding = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(UI_VERTEX_STRIDE)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
            // Color32 字节按 UNORM 解成 0..1（gamma 域预乘 sRGBA）
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R8G8B8A8_UNORM)
                .offset(16),
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
        // 深度读写全关；深度格式仍照实例附件声明（见模块注释 VUID-08914）
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_DST_ALPHA)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        // 颜色 = swapchain 格式；深度格式必须等于实例深度附件（见模块注释）
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(OVERLAY_PIPELINE.color_formats)
            .depth_attachment_format(
                OVERLAY_PIPELINE
                    .depth_format
                    .unwrap_or(vk::Format::UNDEFINED),
            );
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
            "overlay 管线就绪: dynamic rendering（color {:?}，depth {:?}\
             声明对齐读写关），顶点输入 20B（pos/uv/color @ 0/8/16），blend 开（color \
             ONE/ONE_MINUS_SRC_ALPHA，alpha ONE_MINUS_DST_ALPHA/ONE），剔除关，set0 \
             共享常驻表，push 16B（0/8/12）",
            OVERLAY_PIPELINE.color_formats, OVERLAY_PIPELINE.depth_format
        );
        Ok(Self {
            handle: PipelineHandle::new(device, layout, pipeline, OVERLAY_PIPELINE.sets),
        })
    }

    /// 录制投影（帧输入束携带）。
    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.handle.rec()
    }
}
