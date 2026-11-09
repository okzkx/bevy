//! 宿主壳入口：禁渲染 Bevy 宿主 + ash 帧循环。
//!
//! main 只做统筹：决定装哪些插件、禁哪些插件，然后交出调度——**不执行任何
//! Vulkan 调用**。Vulkan 侧的编排（init / draw_frame / teardown）住在
//! [`ash_renderer::driver::host`]；ECS 侧场景机制在 [`ash_renderer::scene`]。
//!
//! 禁用名单 = 渲染族 8 件 + 连带的 `UiPlugin`/`PbrPlugin`。帧循环跑在 bevy runner
//! 里：winit `about_to_wait` 驱动 `app.update()`。失败处理见 host 模块文档与
//! 《错误处理体系：两Tier思想与优雅退出》篇。

use ash_renderer::{
    common::error::{fatal_gate, tier_handler},
    driver::{AshHostPlugin, AshUploadPlugin},
    overlay::OverlayPlugin,
    remote::AshRemotePlugin,
    scene::{
        AshAssetEventPlugin, AshCameraAspectPlugin, AshCameraControlPlugin, AshCameraGuardPlugin,
        AshLedgerPlugin, AshLightControlPlugin, AshMaterialHookPlugin, AshReadinessPlugin,
        AshResidencyPlugin, AshVisibilityExemptPlugin, AshWorkloadPlugin, SceneEntryPlugin,
    },
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
    ui::UiPlugin,
    ui_render::UiRenderPlugin,
};

/// main 返回 AppExit：bevy 已 impl Termination（Success=0/Error=1），失败退出时
/// 调用方能拿到退出码。
fn main() -> AppExit {
    App::new()
        // 统一错误通道：system/observer/command 的 Err 全交 tier_handler 按 Severity
        // 分流；致命错误由 fatal_gate 写 AppExit 退出，挂 First 让它尽早执行。
        .set_error_handler(tier_handler)
        .add_systems(First, fatal_gate)
        .add_plugins(
            DefaultPlugins
                .build()
                // 资产根指到仓库根 assets/（bevy 自带 FlightHelmet 在此）：默认按
                // CARGO_MANIFEST_DIR（cargo run → ash_renderer/）或 exe 目录（直跑 →
                // target/debug/）解析，两种跑法都到不了仓库根，故编译期拼出确定位置。
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
                // bevy_ui 无消费者（交互全走 egui 与 BRP），禁掉省每帧 ui_focus/ui_layout。
                // 坑：GizmoPlugin 不能禁——per-config 内部插件仍注册 update_gizmo_meshes，
                // 禁主插件后 LightGizmoConfigGroup 资源缺席报错；DefaultPickingPlugins
                // 是 PluginGroup，这里无法整族摘除。
                .disable::<UiPlugin>()
                // 坑：PbrPlugin::build 无条件 load_shader_library!，需要 RenderPlugin 注册的
                // Assets<Shader>，禁 RenderPlugin 后必须连带禁它。StandardMaterial 容器与
                // glTF 材质接入由 AshMaterialHookPlugin 手动接线。
                .disable::<PbrPlugin>(),
        )
        // 材质缝接线：init_asset + AshMaterialHook 三钩子进 GltfExtensionHandlers
        .add_plugins(AshMaterialHookPlugin)
        // 宽高比补位：随窗口 resize 修正相机 aspect_ratio
        .add_plugins(AshCameraAspectPlugin)
        // 相机卫兵：Update 幂等清扫不带 CameraOrbit 的外来相机（glTF 内嵌相机等）
        .add_plugins(AshCameraGuardPlugin)
        // 相机轨道控制：左键拖拽环绕 + 滚轮推拉，驱动带 CameraOrbit 的相机
        .add_plugins(AshCameraControlPlugin)
        // 日光方向控制：中键拖拽旋转太阳方位/仰角，驱动带 SunOrbit 的方向光
        .add_plugins(AshLightControlPlugin)
        // 场景进场：load FlightHelmet + spawn WorldAssetRoot + 到货统计
        .add_plugins(SceneEntryPlugin)
        // 稳定槽账本：PostUpdate 传播后按 Changed/Removed 增量维护 Entity→实例数据
        .add_plugins(AshLedgerPlugin)
        // 可见性豁免：场景实体移除 bevy 可见性组件，跳过本渲染器不消费的
        // PostUpdate 可见性计算（约 5.8ms/帧）
        .add_plugins(AshVisibilityExemptPlugin)
        // 压测变更源：BRP 开关控制的每帧旋转（少量变化/全动态两档），默认关
        .add_plugins(AshWorkloadPlugin)
        // 资产事件：AssetEvent::Modified → 记失效凭据 + 引用行重挂脏标（与对象
        // 变更共用同一脏集）
        .add_plugins(AshAssetEventPlugin)
        // 异步就绪：pending 身份账 + AssetLoadFailedEvent（失败进 failed 集不再
        // 重试，行出队由 flush_uploads 裁决）
        .add_plugins(AshReadinessPlugin)
        // 驻留管理器：显存预算 + 按相机请求/淘汰资产（Last 里先于上传，同帧生效；
        // 默认关 = 全部常驻）
        .add_plugins(AshResidencyPlugin)
        // BRP 远程直控：World 经 HTTP JSON-RPC 暴露在 127.0.0.1:15702，
        // 官方 inspection 方法族 + 场景语义自定义方法（查询/改 Transform/相机给值）
        .add_plugins(AshRemotePlugin)
        // egui 调试 UI 的 ECS 侧（裸接 ash）：Update 里输入桥 + pass + 调试窗口，
        // 产 EguiFrame 给帧循环、发布 PointerCapture 给相机/日光控制
        .add_plugins(OverlayPlugin)
        // 上传编排：Last 里盘点账本脏行 → 网格/贴图去重转换 → 合批提交，先于帧循环
        .add_plugins(AshUploadPlugin)
        // 宿主桥：禁渲染补位 + init/draw_frame/teardown 三系统进调度
        .add_plugins(AshHostPlugin)
        .run()
}
