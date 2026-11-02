//! 官方对照参照进程：FlightHelmet 场景交给**官方 bevy_render/wgpu** 渲染，供与
//! ash_renderer 主程序（自研 ash 管线）并排截图对照。
//!
//! 用独立进程而非同进程双渲染器：避免让自研进程初始化 wgpu。本 crate 的 bevy
//! 依赖默认 feature 全开（主程序只是运行时 disable 渲染族插件），所以一个普通
//! example 就是完整的官方渲染宿主，零额外依赖。
//!
//! **本例对照条件**：
//!
//! | 条件 | 本例 |
//! |---|---|
//! | 资产 | FlightHelmet.gltf #Scene0（仓库 assets/），仅头盔，无地面/方块 |
//! | 相机 | (0.7,0.7,1.0) 看 (0,0.3,0)，Perspective 默认 fov |
//! | 分辨率 | 1280×720 |
//! | 方向光 | 20000 lx（FULL_DAYLIGHT），rot ZYX(0,-0.15π,-0.15π)，shadow 关 |
//! | 环境光 | GlobalAmbientLight 默认（80，LightPlugin 预插） |
//! | 材质 | glTF 原样；`OFFICIAL_REF_MODE=unlit` 时全置 unlit |
//! | 输出链 | LDR 直出（无 Hdr）+ Tonemapping::None + DebandDither 关 + Msaa::Off |
//!
//! 坑：主程序现状已与本例不同源——默认场景 Sponza（`--scene` 可换回头盔）、相机为
//! Sponza 中庭机位、环境光 800、输出链 HDR + Bloom + ACES + SMAA；只有方向光照度与
//! 朝向仍一致（`src/scene/content/lights.rs`）。并排对照前须先对齐这些条件，否则差异
//! 不可归因。
//!
//! 跑法：`cargo run -p ash_renderer --example official_reference`；进程 6s 自动
//! 退出，供并排截图脚本定时抓图（坑：按窗口标题 PrintWindow 抓，全屏截图会被
//! 前台应用遮挡）。

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
                        // 与主程序窗口同尺寸（winit 默认 1280×720），显式写出
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

/// 场景装配（Startup）：头盔 + 方向光 + 相机，取值见文件头对照条件表。
fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    // 头盔（无地面/方块——那些是抗锯齿示例的演示物，不属于对照条件）
    commands.spawn(WorldAssetRoot(asset_server.load(
        GltfAssetLabel::Scene(0).from_asset("models/FlightHelmet/FlightHelmet.gltf"),
    )));
    // 方向光：照度/朝向与主程序 lights.rs 一致；shadow 默认即关，显式写出
    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::FULL_DAYLIGHT,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15)),
    ));
    // 相机：输出链钉 LDR + 无 tonemap + 无 dither + 1×，线性值 → sRGB 编码恰好一次
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
/// 置 unlit（跳过光照直出 base color，与主程序 RenderMode::Unlit 同口径）。材质
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
