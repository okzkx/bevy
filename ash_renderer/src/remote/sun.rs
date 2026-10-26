//! 太阳角度读改方法族：`sun_get`（读 SunOrbit 球坐标 + 实际前向）、`sun_set`
//! （按方位/仰角写回）。写路径与中键拖拽共用"球坐标 → 朝向"derive（
//! [`apply_sun_rotation`]），仰角同一限位——BRP 写与拖拽不漂移。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::mechanism::{apply_sun_rotation, clamp_elevation, SunOrbit};

use super::{
    invalid_params,
    params::parse_params_or_default,
    BrpError, BrpResult,
};

/// 方向光实体的球坐标与前向读数（找不到实体 = 场景装配问题）。
///
/// # Errors
/// 方向光实体缺席（Tier② 级状态不可信，协议报 internal）。
#[system]
pub(crate) fn sun_get(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let mut q = world.query_filtered::<(&SunOrbit, &GlobalTransform), With<DirectionalLight>>();
    let Some((orbit, tf)) = q.iter(world).next() else {
        return Err(BrpError::internal("方向光实体缺席——灯光装配未跑或实体被删"));
    };
    Ok(json!({
        "azimuth_deg": orbit.azimuth.to_degrees(),
        "elevation_deg": orbit.elevation.to_degrees(),
        "sun_dir": orbit.sun_dir().to_array(),
        "light_forward": tf.forward().to_array(),
    }))
}

/// `ash_renderer/sun_set` 参数（方位/仰角可选，缺省项保持原值）。
#[derive(serde::Deserialize, Default)]
struct SunSetParams {
    azimuth_deg: Option<f32>,
    elevation_deg: Option<f32>,
}

/// `ash_renderer/sun_set`：按方位角/仰角（度）改太阳方向，缺省项保持原值。
/// 仰角夹在地平线到天顶余量（与拖拽同一限位）；写后同帧生效。
///
/// # Errors
/// 参数缺失/非法数值、方向光实体缺席。
#[system]
pub(crate) fn sun_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SunSetParams = parse_params_or_default(params)?;
    if p.azimuth_deg.is_some_and(|a| !a.is_finite()) || p.elevation_deg.is_some_and(|e| !e.is_finite()) {
        return Err(invalid_params("azimuth_deg/elevation_deg 必须是有限数值"));
    }
    let mut q = world.query_filtered::<(&mut SunOrbit, &mut Transform), With<DirectionalLight>>();
    let Some((orbit, tf)) = q.iter_mut(world).next() else {
        return Err(BrpError::internal("方向光实体缺席——灯光装配未跑或实体被删"));
    };
    let orbit = orbit.into_inner();
    if let Some(az) = p.azimuth_deg {
        orbit.azimuth = az.to_radians();
    }
    if let Some(el) = p.elevation_deg {
        orbit.elevation = clamp_elevation(el.to_radians());
    }
    apply_sun_rotation(orbit, tf.into_inner());
    Ok(json!({
        "azimuth_deg": orbit.azimuth.to_degrees(),
        "elevation_deg": orbit.elevation.to_degrees(),
        "sun_dir": orbit.sun_dir().to_array(),
    }))
}
