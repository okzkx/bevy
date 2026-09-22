//! 灯光组（施工 3.1.3）：方向光实体 spawn + 全局环境光资源核验，零 Vulkan 代码。
//!
//! 环境光是资源不是实体：全局环境光真身 = [`GlobalAmbientLight`] 资源（LightPlugin
//! 预插，默认白光亮度 80），相机组件 AmbientLight 可按相机覆盖——本项目不挂（与
//! 官方默认配置一致），3.5 读 GlobalAmbientLight 进 UBO。方向光沿实体 forward 照射
//!（bevy_light/src/directional_light.rs:25），所以灯只写朝向不写位置。机制与计划
//! §6.4 的订正记录见
//! `.agent/docs/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use std::f32::consts::PI;

use bevy::{
    light::{light_consts, DirectionalLight, GlobalAmbientLight},
    math::EulerRot,
    prelude::*,
};

use super::util::fmt_vec3;

/// 灯光进场（Startup）：方向光一个实体（朝向抄官方 FlightHelmet 示例）；
/// 环境光不 spawn——LightPlugin 已预插全局资源，这里只核验。
pub(super) fn spawn_lights(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::FULL_DAYLIGHT,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15)),
    ));
    info!(
        "灯光进场：方向光 20000 lx（FULL_DAYLIGHT）沿实体 forward；环境光用 GlobalAmbientLight 默认（LightPlugin 预插，不 spawn 实体）"
    );
}

/// 灯光就位核验（Update，报一次即歇）：方向光 1 盏沿实体前向；GlobalAmbientLight
/// 资源在位（3.5 读它进 UBO）。require 链自动插入的 CascadeShadowConfig 等照常
/// 在场，但阴影是渲染语义，本步不取值。
#[derive(Default)]
pub(super) struct LightSetupState {
    done: bool,
}

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
