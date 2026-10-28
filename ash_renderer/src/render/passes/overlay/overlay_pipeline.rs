//! overlay 图形管线：画 egui 镶嵌出的顶点。
//!
//! 与场景管线（[`crate::render::passes::gbuffer::gbuffer_pipeline`]）的关系是"**布局共享、状态各管**"：
//! - **set0 共享**：layout 只声明常驻表 set0（UI 无 UBO，不声明 set1），表在飞
//!   更新对两管线同时生效——"布局=未来管线公共 ABI"的兑现位。切换管线后 set0
//!   需按本布局对象重绑（同句柄、不同 layout 对象）。
//! - **push 16B**：screen_size（points，顶点 pos 同域——tessellate 不按 ppp 缩
//!   放顶点，ppp 只进字形栅格化/像素取整；egui-wgpu 0.36.2 uniform 同名
//!   screen_size_in_points）@0 + tex_index @8 +
//!   sampler_index @12，整帧 UI 一次；与 `overlay_draw.wgsl` 的 PushParams 和
//!   下方 [`UiPushLayout`] 镜像 `offset_of!` 静态互证（三方同源纪律）。
//! - **深度格式声明对齐、读写全关**：渲染实例带 D32 深度附件，本管线声明同格式
//!   才合法——VUID-08914 规定未启用 dynamicRenderingUnusedAttachments 特性时
//!   管线的 depthAttachmentFormat 必须与实例深度附件**相等**（"管线不用即可
//!   UNDEFINED"不成立，首跑被验证层逐 draw 抓获）；深度测试/写入保持关闭 =
//!   UI 不读不写深度（官方 wgpu 后端同款形状）。
//! - **混合开启**（egui 官方 wgpu 后端 0.36.2 判据）：color =
//!   (ONE, ONE_MINUS_SRC_ALPHA)，alpha = (ONE_MINUS_DST_ALPHA, ONE)——egui 顶点
//!   色是预乘，src 因子 ONE 即成；alpha 通式让 alpha 通道走出"预乘 over"。
//! - **剔除 NONE**：epaint 明言 UI 顶点绕向不保证一致（官方同款）。
//!
//! UI 录制契约 [`UiPaint`]/[`UiDrawCall`] 也住本模块：录制层 `render::frame::frame_draw`
//! 与生产方 `overlay::framework::paint` 同层消费，render 不引上层类型。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::{put_f32s, put_u32};
use crate::common::error::Result;

use crate::render::Context;

use crate::render::core::pipeline::graphics::{create_graphics_pipeline, PipelineHandle, PipelineRef};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{stages, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::resident::descriptors::BINDLESS_SET;

/// push 范围覆盖全 16B、双阶段：screen_size 在 vertex 用，槽位在 fragment 用。
pub(crate) static OVERLAY_PIPELINE: PipelineDesc = PipelineDesc {
    label: "overlay_draw",
    shader: &shader_abi::OVERLAY_DRAW,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BINDLESS_SET],
    push: Some(PushRange {
        stages: stages(&[vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]),
        size: UI_PUSH_CONSTANTS_SIZE,
    }),
    color_formats: &[crate::render::SWAPCHAIN_COLOR_FORMAT],
    depth_format: Some(crate::render::core::target::depth::DEPTH_FORMAT),
};

/// push constant 冻结布局（16B）的 Rust 镜像：不参与运行时（字节由 [`pack_ui_push`]
/// 按偏移常量手工排布），存在的意义是让 `offset_of!` 静态断言把 WGSL/管线/CPU
/// 三方钉在一起。
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

/// overlay push constant 总长（pipeline layout 声明 + 排布 + WGSL 三方同源）。
pub(crate) const UI_PUSH_CONSTANTS_SIZE: u32 = 16;

/// UI 顶点步长 20B（epaint 顶点镜像 [`super::ui_vertex_ring::UiVertex`] 的 pos8+uv8+color4；
/// 写入侧的 `size_of` 断言与本常量互证）。
pub(crate) const UI_VERTEX_STRIDE: u32 = 20;

/// overlay push 参数 → 16B 字节（偏移按 [`UiPushLayout`] 静态断言表排布，
/// 标量小端走 [`crate::common::bytes`]；screen_size 是 points 域，与镶嵌顶点
/// 同域——见 [`UiPaint::screen_pt`]）。
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

/// 一个 UI clip 的 draw 参数：UI 环内
/// 引脚 + scissor（物理像素）。宿主侧的组装形态，录制层只消费。
#[derive(Clone, Copy, Debug)]
pub struct UiDrawCall {
    pub index_count: u32,
    pub first_index: u32,
    pub vertex_base: u32,
    /// (x, y, w, h) 物理像素，左上原点——Vulkan scissor 同域。
    pub scissor: [u32; 4],
}

/// 一次 UI 绘制的全部产物（`overlay::framework::paint` 组装，`render::frame::frame_draw` 消费）：
/// 图集槽位 + 屏幕 points 尺寸 + 逐 clip draw 列表；顶点/索引已由生产方写入 UI 环
/// 本帧槽 buffer，draw 用环内引脚定位。
pub struct UiPaint {
    /// 图集纹理槽（push 的 tex_index）。
    pub atlas_texture: u32,
    /// 图集采样器槽（push 的 sampler_index；键与 fallback 同键去重）。
    pub atlas_sampler: u32,
    /// 屏幕 points 尺寸（push 的 screen_size；tessellate 顶点 pos 同域，ppp 只
    /// 进字形栅格化/像素取整——渲染目标尺寸按 ppp 换算是消费端职责，本字段已除）。
    pub screen_pt: [f32; 2],
    /// 逐 clip draw（mesh 边界即 clip 边界，scissor 各自持有）。
    pub draws: Vec<UiDrawCall>,
}

/// overlay 图形管线 + pipeline layout（只声明 set0 + 16B push）。
#[derive(bevy::prelude::Resource)]
pub struct OverlayPipeline {
    handle: PipelineHandle,
}

impl OverlayPipeline {
    /// 按 [`OVERLAY_PIPELINE`] 建管线（set0 + 16B push；骨架见 `PipelineDesc::build`），
    /// 颜色 + 深度格式声明，深度读写全关。任何失败回收已建对象（全有或全无）。
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
        // 顶点输入 = epaint 顶点镜像的 20B 交错契约（pos/uv/color @ 0/8/16）
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
            // Color32 字节 → UNORM 解包即 /255 的 0..1（gamma 预乘 sRGBA）
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
            // epaint 明言 UI 顶点绕向不保证一致，剔除保持 NONE（官方同款）
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // depth-stencil 全关（UI 不读不写深度）；深度格式仍须随渲染实例声明
        //（VUID-08914，管线声明面 = 实例附件全集的形状声明）
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            // egui 顶点色预乘 → src 用 ONE（官方 wgpu 后端 0.36.2 判据）
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_DST_ALPHA)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        // dynamic rendering 渲染信息：颜色 = swapchain SRGB view 同格式；深度
        // 格式与实例附件**全等声明**——VUID-08914：未启用
        // dynamicRenderingUnusedAttachments 特性时管线的 depthAttachmentFormat
        // 必须等于实例深度附件格式（"不用即可 UNDEFINED"不成立，depth 读/写
        // 关闭才是"不用"的表达位）
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(OVERLAY_PIPELINE.color_formats)
            .depth_attachment_format(OVERLAY_PIPELINE.depth_format.expect("overlay 声明深度"));
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
            OVERLAY_PIPELINE.color_formats,
            OVERLAY_PIPELINE.depth_format
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
