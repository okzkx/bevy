//! 宿主壳入口（step2 施工项②③④）：禁渲染 Bevy 宿主 + ash 清屏帧循环。
//!
//! main 只做统筹：决定装哪些插件、禁哪些插件，然后交出调度——**不执行任何
//! Vulkan 调用**。Vulkan 侧的编排（init / draw_frame / teardown）住在
//! [`ash_renderer::host`]（步骤 3 开工前的结构整理：五段施工都要长帧循环，
//! 装配与编排一次分家）；ECS 侧材质缝在 [`ash_renderer::scene`]。
//!
//! 禁用名单 = step1《DefaultPlugins分类.md》§1 的渲染族 8 件
//! （0.20.0-dev 下路径逐一复核未变，见 .agent/docs/2-宿主壳/2-宿主壳搭建记录.md §2）。
//! 帧循环住在 bevy runner 里（侦察篇 §4）：winit `about_to_wait` 驱动 `app.update()`。
//! 失败策略（两 Tier，非必要不 panic）见 host 模块文档与《错误处理体系：两Tier思想与优雅退出》篇。

use ash_renderer::{
    host::AshHostPlugin,
    scene::{AshCollectPlugin, AshMaterialHookPlugin, SceneEntryPlugin},
    upload::AshUploadPlugin,
};
use bevy::{
    anti_alias::AntiAliasPlugin,
    core_pipeline::CorePipelinePlugin,
    gizmos_render::GizmoRenderPlugin,
    pbr::PbrPlugin,
    post_process::PostProcessPlugin,
    prelude::*,
    render::{pipelined_rendering::PipelinedRenderingPlugin, RenderPlugin},
    sprite_render::SpriteRenderPlugin,
    ui_render::UiRenderPlugin,
};

/// main 返回 AppExit：bevy 已 impl Termination（AppExit → ExitCode，Success=0/Error=1），
/// 失败路径的"优雅"才算闭环（调用方能拿到退出码）
fn main() -> AppExit {
    App::new()
        .add_plugins(
            DefaultPlugins
                .build()
                // 资产根指到仓库根 assets/（bevy 自带 FlightHelmet 在此）：默认按
                // CARGO_MANIFEST_DIR（cargo run → ash_renderer/）或 exe 目录（直跑 →
                // target/debug/）解析，两种跑法都到不了仓库根，故编译期拼出确定位置。
                // 装配级配置，属 main 的统筹地盘。
                .set(AssetPlugin {
                    file_path: format!("{}/../assets", env!("CARGO_MANIFEST_DIR")),
                    ..default()
                })
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
                // 与 glTF 材质接入缝已由 AshMaterialHookPlugin 手动接线（scene 模块）。
                .disable::<PbrPlugin>(),
        )
        // 材质缝接线（step3 任务 3.1.1）：init_asset + AshMaterialHook 三钩子进 GltfExtensionHandlers
        .add_plugins(AshMaterialHookPlugin)
        // 场景进场（step3 任务 3.1.2）：load FlightHelmet + spawn WorldAssetRoot + 到货统计
        .add_plugins(SceneEntryPlugin)
        // 场景采集（step3 任务 3.1.4）：PostUpdate 帧末直读 primitive 三样，产 CollectedScene 快照
        .add_plugins(AshCollectPlugin)
        // buffer 侧上传（3.2.4）：Last 里快照去重 → 合批进 GPU 池,先于帧循环
        .add_plugins(AshUploadPlugin)
        // 宿主桥：禁渲染补位 + init/draw_frame/teardown 三系统进调度
        .add_plugins(AshHostPlugin)
        .run()
}
