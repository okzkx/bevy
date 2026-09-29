//! 图形管线（施工 3.4.2）：dynamic rendering 图形管线 + pipeline layout（set0/set1 + push）。
//!
//! 管线吃的是 3.3 冻结的三方契约：set0/set1 layout 来自 [`super::BindlessTables`]
//!（"集与布局同源，不可另建"），push constant range 按 96B 冻结表（偏移
//! 0/64/68/80，与 `debug_draw.wgsl` 的 PushParams 及下方 [`PushLayout`] 镜像
//! `offset_of!` 静态互证——"repr(C) 或低于 128B 都不能独立证明对齐"，断言在
//! 编译期执法）。
//!
//! 状态选型（3.4 面板定案）：
//! - **顶点输入**：交错 32B（pos12+normal12+uv8，[`super::VERTEX_STRIDE`]），与
//!   3.2 mesh_convert 是同一份契约；vertex-input 布局不默认等同 storage 布局。
//! - **reverse-Z 成套**：bevy 透视投影是无限反向 Z（`get_clip_from_view`），
//!   深度清值 0 + `GREATER` 比较成套使用，不混正向 Z 的 LESS 模板。
//! - **viewport 直出**（3.4 朝向定案）：bevy 投影走 glam 的 directx 变体（Y-up
//!   NDC、Z ∈ [0,1]，`bevy_math` 重出口）；E5 光栅级实验（顶点直出已知 clip 坐
//!   标）与官方 color_grading 示例同相机对照截图共同实证：本机 Vulkan **正高度
//!   viewport 直出即与官方 wgpu 渲染同向**，负高度翻转反而上下颠倒——"教科书式
//!   Vulkan 需要翻转"的直觉在此不成立，以实测为准（证据链见施工记录 §viewport）。
//!   正高度下屏幕绕向未被镜像，M2 仍先关背面剔除（面板：HoseMat 双面、镜片按
//!   不透明覆盖），开剔除前的绕向核验另立判定。
//! - **混合关闭**：不透明调试绘制，alpha 写出不参与混合（LensesMat 的 BLEND
//!   按 opaque 覆盖，材质覆盖策略记录在施工记录）。
//! - **动态 viewport/scissor**：随 swapchain 尺寸逐帧设置，重建不重建管线。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::VulkanError;

use super::Context;

/// shader 模块字节（build.rs 产物，OUT_DIR 之下，与探针同一编译路径）。
const DEBUG_DRAW_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/debug_draw.spv"));

/// push constant 冻结布局（96B）的 Rust 镜像：本类型不参与运行时（字节由
/// [`pack_push`] 按偏移常量手工排布，不依赖 repr 对齐的巧合），存在的意义是
/// 让 `offset_of!`/`size_of` 静态断言把 WGSL/管线/CPU 三方钉在一起。
#[repr(C)]
#[allow(dead_code)]
struct PushLayout {
    model: [f32; 16],
    tex_index: u32,
    sampler_index: u32,
    /// 68→80 的对齐垫：vec4f 的 16B 对齐在 WGSL 端自然产生，镜像端显式写出
    _pad: [u32; 2],
    base_color: [f32; 4],
}

const _: () = {
    assert!(std::mem::offset_of!(PushLayout, model) == 0);
    assert!(std::mem::offset_of!(PushLayout, tex_index) == 64);
    assert!(std::mem::offset_of!(PushLayout, sampler_index) == 68);
    assert!(std::mem::offset_of!(PushLayout, base_color) == 80);
    assert!(std::mem::size_of::<PushLayout>() == 96);
};

/// push constant 总长（pipeline layout 声明 + [`pack_push`] 排布 + WGSL 侧
/// PushParams，三方同源）。
pub const PUSH_CONSTANTS_SIZE: u32 = 96;

/// 一个 draw 的 push 参数（宿主侧取数产物，[`super::frames::DrawCall`] 的成员）。
#[derive(Clone, Copy, Debug)]
pub struct PushData {
    /// local→world（GlobalTransform 终值，列主序展平见 `to_cols_array`）。
    pub model: [f32; 16],
    /// set0 b0 纹理数组下标（缺贴图走 fallback 槽 0）。
    pub tex_index: u32,
    /// set0 b1 采样器数组下标。
    pub sampler_index: u32,
    /// 材质基色（线性域；与采样端解码互补，编码只在输出端做一次）。
    pub base_color: [f32; 4],
}

/// [`PushData`] → 96B push 字节（偏移按 [`PushLayout`] 的静态断言表手工排布）。
#[must_use]
pub fn pack_push(d: &PushData) -> [u8; PUSH_CONSTANTS_SIZE as usize] {
    let mut out = [0u8; PUSH_CONSTANTS_SIZE as usize];
    let mut put = |offset: usize, bytes: &[u8]| {
        out[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    for (i, v) in d.model.iter().enumerate() {
        put(i * 4, &v.to_le_bytes());
    }
    put(64, &d.tex_index.to_le_bytes());
    put(68, &d.sampler_index.to_le_bytes());
    // 72..80 是对齐垫，保持 0
    for (i, v) in d.base_color.iter().enumerate() {
        put(80 + i * 4, &v.to_le_bytes());
    }
    out
}

/// 图形管线 + pipeline layout（dynamic rendering 形态，无 RenderPass）。
#[derive(bevy::prelude::Resource)]
pub struct GraphicsPipeline {
    device: Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl GraphicsPipeline {
    /// 建管线链：shader module（build.rs 补丁产物）→ pipeline layout（双 set +
    /// push range）→ GraphicsPipelineCreateInfo（dynamic rendering 附件格式声明）。
    /// 任何失败回收已建对象（全有或全无）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        color_format: vk::Format,
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, VulkanError> {
        let device = &ctx.device;
        // 词流（小端）→ ash 的 &[u32]；build.rs 只写整词，as_chunks 即全部字节
        let words: Vec<u32> = DEBUG_DRAW_SPV
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
                color_format,
                depth_format,
                set0_layout,
                set1_layout,
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
        color_format: vk::Format,
        depth_format: vk::Format,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
        module: vk::ShaderModule,
    ) -> Result<Self, VulkanError> {
        unsafe {
            // push range 覆盖全 96B、双 stage：model 在 vertex 用，其余在 fragment 用
            let push_range = vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(PUSH_CONSTANTS_SIZE);
            let set_layouts = [set0_layout, set1_layout];
            let layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(std::slice::from_ref(&push_range)),
                None,
            )?;
            let result = Self::create_pipeline(device, color_format, depth_format, layout, module);
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
        color_format: vk::Format,
        depth_format: vk::Format,
        layout: vk::PipelineLayout,
        module: vk::ShaderModule,
    ) -> Result<Self, VulkanError> {
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
            // 顶点输入 = 3.2 mesh_convert 的同一份 32B 交错契约（三属性偏移 0/12/24）
            let vertex_binding = [vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(super::VERTEX_STRIDE as u32)
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
            // viewport/scissor 动态：尺寸随 swapchain 逐帧给（负高度翻转见模块注释）
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                // M2 关剔除：负 viewport 镜像绕向待核验 + HoseMat 双面 + 镜片按
                // 不透明覆盖——三者齐备后才选正面方向与剔除变体（面板定案）
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
            let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(false)
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(&blend_attachment);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic_info =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            // dynamic rendering：附件格式内联声明（无 RenderPass/Framebuffer）
            let color_formats = [color_format];
            let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
                .color_attachment_formats(&color_formats)
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
            let pipeline = device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)?
                .remove(0);
            // module 用毕即拆：管线创建完成即编译收口，规范允许此刻销毁 shader
            // module（vkDestroyShaderModule 与管线无生命周期耦合）。3.4 首版漏了
            // 这一步（句柄是纯整数，没人存没人拆），每次启动泄漏 1 个 VkShaderModule
            //——3.5 回归由验证层 VUID-vkDestroyDevice-device-05137 抓获（D7）。
            device.destroy_shader_module(module, None);
            info!(
                "图形管线就绪: dynamic rendering（color {color_format:?} / depth {depth_format:?}），\
                 顶点输入 32B 交错（pos/normal/uv @ 0/12/24），reverse-Z（clear 0 + GREATER），\
                 viewport 负高度翻转，剔除关，push 96B（0/64/68/80），set0/set1 来自常驻表"
            );
            Ok(Self {
                device: device.clone(),
                layout,
                pipeline,
            })
        }
    }

    /// 管线句柄（帧录制绑定用）。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }

    /// pipeline layout 句柄（push/描述符绑定与管线同源）。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }
}

// # Safety:句柄本质是整数,创建/销毁只在持有方线程;方法只读句柄(与 FramePool 同款约定)
unsafe impl Send for GraphicsPipeline {}
unsafe impl Sync for GraphicsPipeline {}

impl Drop for GraphicsPipeline {
    fn drop(&mut self) {
        // 契约边界:不等 GPU,退出排空由 teardown 的 device_wait_idle 先行(D4);
        // pipeline 先于 layout(引用方向),layout 先于它借用的 set layouts(在
        // BindlessTables 里,拆除序由 host 的 remove_resource 顺序保证)
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}
