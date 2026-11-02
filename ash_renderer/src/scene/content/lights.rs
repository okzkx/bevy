//! 灯光组（业务半边）：方向光实体 spawn + 全局环境光资源覆盖，零 Vulkan 代码。
//!
//! 环境光是资源不是实体：灯光 UBO 组装读 [`GlobalAmbientLight`] 资源（相机组件
//! AmbientLight 本项目不挂）。方向光沿实体 forward 照射，所以灯只写朝向不写位置。
//! 机制见 `.agents/docs/开发步骤/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use std::f32::consts::PI;

use bevy::{
    light::{light_consts, DirectionalLight, GlobalAmbientLight},
    math::EulerRot,
    prelude::*,
};

use super::fmt_vec3;
use crate::scene::mechanism::SunOrbit;

use ash_macros::system;

/// 灯光进场（Startup）：方向光一个实体（朝向与官方对照进程同源），挂上由进场
/// 朝向反解的 [`SunOrbit`] 供中键拖拽驱动（首次拖拽前朝向不变）。
/// 坑：LightPlugin 默认环境光 80 对 20000 lx 满日照是 1:250 失衡，室内只剩环境项
/// 时黑死；覆盖为 800（≈直射 4% 的天空补光，无 IBL 时环境项是唯一补光）。
#[system]
pub(super) fn spawn_lights(mut commands: Commands) {
    commands.insert_resource(GlobalAmbientLight {
        brightness: 800.0,
        ..default()
    });
    let rotation = Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15);
    commands.spawn((
        DirectionalLight {
            illuminance: light_consts::lux::FULL_DAYLIGHT,
            ..default()
        },
        Transform::from_rotation(rotation),
        // 太阳方向 = 灯光 forward 的反向（沿 forward 照射）。
        SunOrbit::from_dir(-(rotation * Vec3::NEG_Z)),
    ));
    info!(
        "灯光进场：方向光 20000 lx（FULL_DAYLIGHT）沿实体 forward；环境光覆盖为 800（满日照 4% 天空补光，替换 LightPlugin 默认 80）"
    );
}

/// 灯光就位核验（Update，只跑一次）：方向光 1 盏沿实体前向；GlobalAmbientLight
/// 资源在位（灯光 UBO 组装读它）。
#[system]
pub(super) fn report_lights_setup(
    lights: Query<&GlobalTransform, With<DirectionalLight>>,
    ambient: Option<Res<GlobalAmbientLight>>,
) {
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
