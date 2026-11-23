//! 程序化天空方法族：`sky_set`（天空参数写入，缺省字段不动）与 `sky_stats`（当前
//! 值只读回显）。天空面板写同一份 [`SkySettings`]，`draw_frame` 每帧照资源写帧 UBO，
//! 改参下一帧生效。

use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::SkySettings;

use crate::remote::common::params::{invalid_params, parse_params, res, res_mut};

/// 太阳角半径上限（弧度，π/2）：再大就没有"圆盘"语义。
const MAX_SUN_RADIUS: f32 = std::f32::consts::FRAC_PI_2;

/// 取可选数值字段并做有限性校验；字段缺席返回 `None`（缺省字段不动）。
fn opt_f32(
    p: &serde_json::Map<String, Value>,
    key: &str,
    name: &str,
) -> Result<Option<f32>, BrpError> {
    let Some(v) = p.get(key) else {
        return Ok(None);
    };
    let f = v
        .as_f64()
        .ok_or_else(|| invalid_params(format!("{name}（{key}）非数值")))? as f32;
    if !f.is_finite() {
        return Err(invalid_params(format!("{name}（{key}）须为有限值")));
    }
    Ok(Some(f))
}

/// 可省标量字段写入 + 回显；`rule` 为取值约束（判据与提示同源）。
fn set_f32(
    echo: &mut Value,
    p: &serde_json::Map<String, Value>,
    key: &str,
    target: &mut f32,
    name: &str,
    ok: impl Fn(f32) -> bool,
    rule: &str,
) -> Result<(), BrpError> {
    let Some(f) = opt_f32(p, key, name)? else {
        return Ok(());
    };
    if !ok(f) {
        return Err(invalid_params(format!("{name}（{key}）{rule}")));
    }
    *target = f;
    echo[key] = json!(f);
    Ok(())
}

/// 可省颜色的校验 + 写入 + 回显（长度 3、非负有限）。
fn set_rgb(
    echo: &mut Value,
    p: &serde_json::Map<String, Value>,
    key: &str,
    target: &mut [f32; 3],
    name: &str,
) -> Result<(), BrpError> {
    let Some(v) = p.get(key) else {
        return Ok(());
    };
    let arr = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| invalid_params(format!("{name}（{key}）需要长度 3 的数组")))?;
    let mut rgb = [0.0f32; 3];
    for (i, c) in arr.iter().enumerate() {
        let f = c
            .as_f64()
            .ok_or_else(|| invalid_params(format!("{name}（{key}）[{i}] 非数值")))?
            as f32;
        if !f.is_finite() || f < 0.0 {
            return Err(invalid_params(format!(
                "{name}（{key}）[{i}] 须为非负有限值"
            )));
        }
        rgb[i] = f;
    }
    *target = rgb;
    echo[key] = json!(rgb);
    Ok(())
}

/// `ash_renderer/sky_set`：程序化天空参数。
/// 请求 `{"enabled": bool?, "zenith": [r,g,b]?, "horizon": [r,g,b]?,
/// "ground": [r,g,b]?, "intensity": f32?, "horizon_exponent": f32?,
/// "ground_exponent": f32?, "sun_radius": f32?, "sun_gain": f32?,
/// "glow_intensity": f32?, "glow_falloff": f32?}`——缺省字段不动。
#[system]
pub(crate) fn sky_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> = parse_params(
        params,
        "params 缺失：需要 enabled/zenith/horizon/ground/intensity/… 之一",
    )?;
    let mut sky = res_mut::<SkySettings>(world)?;
    let mut echo = json!({});
    if let Some(v) = p.get("enabled") {
        let enabled = v
            .as_bool()
            .ok_or_else(|| invalid_params("enabled 非布尔"))?;
        sky.enabled = enabled;
        echo["enabled"] = json!(enabled);
    }
    set_rgb(&mut echo, &p, "zenith", &mut sky.zenith, "天顶色")?;
    set_rgb(&mut echo, &p, "horizon", &mut sky.horizon, "地平线色")?;
    set_rgb(&mut echo, &p, "ground", &mut sky.ground, "地面色")?;
    set_f32(
        &mut echo,
        &p,
        "intensity",
        &mut sky.intensity,
        "天空强度",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "horizon_exponent",
        &mut sky.horizon_exponent,
        "地平线→天顶过渡幂",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "ground_exponent",
        &mut sky.ground_exponent,
        "地平线→地面过渡幂",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "sun_radius",
        &mut sky.sun_radius,
        "太阳角半径",
        |v| v > 0.0 && v < MAX_SUN_RADIUS,
        "须落在 (0, π/2)",
    )?;
    set_f32(
        &mut echo,
        &p,
        "sun_gain",
        &mut sky.sun_gain,
        "太阳圆盘增益",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "glow_intensity",
        &mut sky.glow_intensity,
        "光晕强度",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "glow_falloff",
        &mut sky.glow_falloff,
        "光晕衰减幂",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params(
            "无有效字段（enabled/zenith/horizon/ground/intensity/… 任一）",
        ));
    }
    Ok(json!({ "applied": echo, "current": current(&sky) }))
}

/// `ash_renderer/sky_stats`：当前天空参数只读回显。
#[system]
pub(crate) fn sky_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    Ok(current(res::<SkySettings>(world)?))
}

/// 天空参数现值（`sky_set` 回显与 `sky_stats` 共用）。
fn current(sky: &SkySettings) -> Value {
    json!({
        "enabled": sky.enabled,
        "zenith": sky.zenith,
        "horizon": sky.horizon,
        "ground": sky.ground,
        "intensity": sky.intensity,
        "horizon_exponent": sky.horizon_exponent,
        "ground_exponent": sky.ground_exponent,
        "sun_radius": sky.sun_radius,
        "sun_gain": sky.sun_gain,
        "glow_intensity": sky.glow_intensity,
        "glow_falloff": sky.glow_falloff,
    })
}
