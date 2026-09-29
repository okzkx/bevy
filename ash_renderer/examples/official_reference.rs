//! 3.5.3 官方对照参照进程：同一 FlightHelmet 场景交给**官方 bevy_render/wgpu**
//! 渲染，与 ash_renderer 主程序（自研 ash 管线）做并排受控对照（3.5.3）。
//!
//! 独立进程而非同进程双渲染器——面板定案"避免让自研进程初始化 wgpu"。本
//! crate 的 bevy 依赖默认 feature 本就全开（主程序只是运行时 disable 渲染族
//! 插件），所以一个普通 example 就是完整的官方渲染宿主，零额外依赖。
//!
//! **对照条件表**（与 ash_renderer 主程序逐项同源，收官记录引用本表）：
//!
//! | 条件 | 本例 | ash_renderer 主程序 |
//! |---|---|---|
//! | 资产 | FlightHelmet.gltf #Scene0（仓库 assets/） | 同 |
//! | 场景 | 仅头盔（六 primitive，无地面/方块） | 同 |
//! | 相机 | (0.7,0.7,1.0) 看 (0,0.3,0)，Perspective 默认 fov | 同（scene/camera.rs 同参数） |
//! | 分辨率 | 1280×720 | 同（winit 默认） |
//! | 方向光 | 20000 lx（FULL_DAYLIGHT），rot ZYX(0,-0.15π,-0.15π)，shadow 关 | 同（scene/lights.rs 同参数） |
//! | 环境光 | GlobalAmbientLight 默认（80，LightPlugin 预插） | 同（3.5.1 读进 UBO） |
//! | 材质 | glTF 原样 + `OFFICIAL_REF_MODE=unlit` 时全置 unlit | 不透明调试覆盖 + mode 三态 |
//! | 输出链 | LDR 直出（无 Hdr）+ Tonemapping::None + DebandDither 关 + Msaa::Off | 线性着色 → SRGB view 编码 |
//!
//! 输出端约定：两侧最终都走"线性值 → sRGB 编码恰好一次"；Tonemapping::None
//! 让官方侧不做 tonemap（默认 TonyMcMapface 是两侧行数不同的一处已知差异），
//! 亮侧超 1 的部分同样线性削顶——方向/明暗界线可判读，绝对亮度不对齐（待决）。
//!
//! 跑法：`cargo run -p ash_renderer --example official_reference`；进程 6s 自动
//! 退出，供并排截图脚本定时抓图（PrintWindow 按标题，全屏截图会被前台应用
//! 遮挡——3.2 前置闸门的方法论钉子）。

use std::f32::consts::PI;

use bevy::{
    camera::Camera3d,
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    gltf::GltfAssetLabel,
    light::{light_consts, DirectionalLight},
    prelude::*,
    render::view::Msaa,
    window::{WindowPlugin, WindowResolution},
    world_serialization::WorldAssetRoot,
};

/// 对照进程存活时长（Startup 起计时，到点 `AppExit::Success` 自动退出）——
/// 截图脚本在 2~4s 窗口抓图（场景加载 + 首画稳定需要一点时间）。
const LIFETIME_SECS: f32 = 6.0;

fn main() -> AppExit {
    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        // 与主程序同尺寸（写显是对照条件表的一部分）
                        resolution: WindowResolution::new(1280, 720),
                        title: "official-reference".into(),
                        ..default()
                    }),
                    ..default()
                })
                // 资产根指仓库根 assets/（主程序 main.rs 同款编译期拼法）
                .set(AssetPlugin {
                    file_path: format!("{}/../assets", env!("CARGO_MANIFEST_DIR")),
                    ..default()
                }),
        )
        .add_systems(Startup, setup)
        .add_systems(Update, (apply_unlit_override, auto_exit))
        .run()
}

/// 场景装配（Startup）：头盔 + 方向光 + 相机，参数抄主程序 scene/ 与官方
/// anti_aliasing.rs 的同一组值——条件表三行同源。
fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    // 头盔（无地面/方块——那些是抗锯齿示例的演示物，不属于对照条件）
    commands.spawn(WorldAssetRoot(asset_server.load(
        GltfAssetLabel::Scene(0).from_asset("models/FlightHelmet/FlightHelmet.gltf"),
    )));
    // 方向光：与 scene/lights.rs 同参数（shadow 默认即关，写显是条件表）
    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::FULL_DAYLIGHT,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15)),
    ));
    // 相机：与 scene/camera.rs 同取景；输出链钉 LDR+无 tonemap+无 dither+1×，
    // 与自研侧"线性着色 → sRGB 编码恰好一次"同形
    commands.spawn((
        Camera3d::default(),
        Msaa::Off,
        Tonemapping::None,
        DebandDither::Disabled,
        Transform::from_xyz(0.7, 0.7, 1.0).looking_at(Vec3::new(0.0, 0.3, 0.0), Vec3::Y),
    ));
    info!("官方对照场景就绪：头盔 + 20000lx 方向光 + 同取景相机（LDR/None/Off）");
}

/// unlit 覆盖（`OFFICIAL_REF_MODE=unlit` 时启用）：容器内全部 StandardMaterial
/// 置 unlit（bevy 侧跳过光照直出 base color，与主程序 mode=1 同口径）。材质
/// 异步到货——每帧幂等重扫，新到货的材质下一帧被覆盖；未启用时只报一次。
fn apply_unlit_override(
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut settled: Local<bool>,
) {
    if std::env::var("OFFICIAL_REF_MODE").ok().as_deref() != Some("unlit") {
        if !*settled {
            *settled = true;
            info!("官方对照模式：glTF 原样材质（OFFICIAL_REF_MODE 未设 unlit）");
        }
        return;
    }
    let mut flipped = 0usize;
    for (_, material) in materials.iter_mut() {
        if !material.unlit {
            material.unlit = true;
            flipped += 1;
        }
    }
    if flipped > 0 {
        info!("官方对照 unlit 覆盖：+{flipped}（容器 {} 个材质）", materials.len());
    }
}

/// 自动退出：LIFETIME_SECS 到点写 `AppExit::Success`（对照进程不需要人工关闭）。
fn auto_exit(time: Res<Time>, mut elapsed: Local<f32>, mut exit: MessageWriter<AppExit>) {
    *elapsed += time.delta_secs();
    if *elapsed >= LIFETIME_SECS {
        exit.write(AppExit::Success);
    }
}
