//! 灯光组（业务半边）：方向光实体 spawn + 全局环境光资源覆盖，零 Vulkan 代码。
//!
//! 环境光是资源不是实体：全局环境光真身 = [`GlobalAmbientLight`] 资源（LightPlugin
//! 预插默认白光亮度 80），这里显式覆盖为与满日照方向光匹配的补光值（默认 80 对
//! 20000 lx 直射失衡，见 spawn_lights 注释），相机组件 AmbientLight 可按相机覆盖
//! ——本项目不挂，灯光 UBO 组装读 GlobalAmbientLight。方向光沿实体 forward
//! 照射（bevy_light/src/directional_light.rs:25），所以灯只写朝向不写位置。机制
//! 见 `.agents/docs/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use std::f32::consts::PI;

use bevy::{
    light::{light_consts, DirectionalLight, GlobalAmbientLight},
    math::EulerRot,
    prelude::*,
};

use crate::scene::util::fmt_vec3;

use ash_macros::system;

/// 灯光进场（Startup）：方向光一个实体（朝向与官方对照进程同源）；
/// 环境光显式覆盖 LightPlugin 预插的默认值——bevy 默认亮度 80 是无主场景
/// 兜底，对 20000 lx 满日照直射是 1:250 失衡（室内只剩环境项时黑死，8.4.1
/// 实抓）。800 ≈ 直射日 4% 的天空补光，室内中灰面 ≈0.13 HDR（ACES 后 sRGB
/// ≈0.40，暗部可读且与受光面保留对比）；步骤 9 IBL 落地后由真间接光接替。
#[system]
pub(super) fn spawn_lights(mut commands: Commands) {
    commands.insert_resource(GlobalAmbientLight {
        brightness: 800.0,
        ..default()
    });
    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::FULL_DAYLIGHT,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15)),
    ));
    info!(
        "灯光进场：方向光 20000 lx（FULL_DAYLIGHT）沿实体 forward；环境光覆盖为 800（满日照 4% 天空补光，替换 LightPlugin 默认 80）"
    );
}

/// 灯光就位核验（Update，报一次即歇）：方向光 1 盏沿实体前向；GlobalAmbientLight
/// 资源在位（灯光 UBO 组装读它）。require 链自动插入的 CascadeShadowConfig 等照常
/// 在场，但阴影是渲染语义，不取值。
#[derive(Default)]
pub(super) struct LightSetupState {
    done: bool,
}

#[system]
pub(super) fn report_lights_setup(
    mut state: Local<LightSetupState>,
    lights: Query<&GlobalTransform, With<DirectionalLight>>,
    ambient: Option<Res<GlobalAmbientLight>>,
) {
    if state.done {
        return;
    }
    state.done = true;
    let ambient_note = match &ambient {
        Some(a) => format!(
            "环境光 GlobalAmbientLight 亮度 {}（LightPlugin 预插默认，相机组件 AmbientLight 可覆盖）",
            a.brightness
        ),
        None => "环境光 GlobalAmbientLight 资源缺失（LightPlugin 未跑？）".to_string(),
    };
    let light_count = lights.iter().count();
    let Ok(light_tf) = lights.single() else {
        warn!(
            "灯光就位核验未过：方向光 {light_count} 盏（期望 1）；{ambient_note}"
        );
        return;
    };
    info!(
        "灯光就位：方向光 {light_count} 盏沿前向 {}；{ambient_note}",
        fmt_vec3(*light_tf.forward()),
    );
}
