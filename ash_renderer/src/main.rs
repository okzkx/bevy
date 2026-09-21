//! 宿主壳入口（step2 施工项②）：组装禁渲染 Bevy，保留 winit 窗口与事件循环。
//!
//! 禁用名单 = step1《DefaultPlugins分类.md》§1 的渲染族 8 件
//! （0.20.0-dev 下路径逐一复核未变，见 .agent/docs/step2-宿主壳/宿主壳搭建记录.md §2）。
//! ash 清屏循环在施工项③④接入。

use bevy::{
    anti_alias::AntiAliasPlugin,
    core_pipeline::CorePipelinePlugin,
    gizmos_render::GizmoRenderPlugin,
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    pbr::PbrPlugin,
    post_process::PostProcessPlugin,
    prelude::*,
    render::{pipelined_rendering::PipelinedRenderingPlugin, RenderPlugin},
    sprite_render::SpriteRenderPlugin,
    ui_render::UiRenderPlugin,
};

fn main() {
    App::new()
        // 0.20 文档化的用户责任（bevy_image/src/image.rs:2467）：该资源原本由 RenderPlugin
        // 的 wgpu finish() 从 device.features() 生成，禁渲染后须自报。NONE = 诚实初值——
        // step3 接 ash 后按实际查询到的 VkFormat 支持改写。
        .insert_resource(CompressedImageFormatSupport(CompressedImageFormats::NONE))
        .add_plugins(
            DefaultPlugins.build()
                .disable::<RenderPlugin>()
                .disable::<PipelinedRenderingPlugin>()
                .disable::<CorePipelinePlugin>()
                .disable::<PostProcessPlugin>()
                .disable::<AntiAliasPlugin>()
                .disable::<SpriteRenderPlugin>()
                .disable::<UiRenderPlugin>()
                .disable::<GizmoRenderPlugin>()
                // 连带禁项（0.20.0-dev 实测）：PbrPlugin::build 无条件 load_shader_library!
                // 分配 Handle<Shader>，而 Assets<Shader> 的注册在 RenderPlugin::build（bevy_render/src/lib.rs:385）
                // ——禁 RenderPlugin 后无人注册，PbrPlugin 必炸。材质容器（Assets<StandardMaterial>）
                // 与 glTF 材质接入缝留给 step3 手动接线。
                .disable::<PbrPlugin>(),
        )
        .add_systems(Startup, announce)
        .add_systems(Update, frame_heartbeat)
        .run();
}

fn announce() {
    info!("宿主壳启动：渲染族 8 插件已禁用，无 RenderApp / 无 wgpu 初始化");
}

/// ash 清屏循环（施工项④）接管前的帧心跳，证明 winit runner 在持续驱动 update。
fn frame_heartbeat(mut frames: Local<u32>) {
    *frames += 1;
    if *frames % 1000 == 0 {
        info!("帧心跳 {}", *frames);
    }
}
