//! 物理天空方法族：`sky_set`（大气参数写入，缺省字段不动）与 `sky_stats`（当前
//! 值只读回显）。天空面板写同一份 [`SkySettings`]，`draw_frame` 每帧照资源写帧 UBO，
//! 改参下一帧生效。
//!
//! 单位：长度/高度用米，散射与吸收系数用 1/米（量级 1e-6，面板里按 ×1e-6 显示），
//! 角度用弧度。

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

/// 可省三分量字段（非负有限）的校验 + 写入 + 回显。
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

/// `ash_renderer/sky_set`：物理天空参数。请求字段全部可省，缺省不动：
/// `{"enabled", "intensity", "planet_radius", "atmosphere_height",
/// "rayleigh":[r,g,b], "rayleigh_scale_height", "mie", "mie_scale_height",
/// "mie_anisotropy", "ozone":[r,g,b], "ozone_center", "ozone_width",
/// "ground_albedo":[r,g,b], "sun_gain", "sun_radius"}`。
#[system]
pub(crate) fn sky_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> = parse_params(
        params,
        "params 缺失：需要 enabled/intensity/rayleigh/mie/ozone/… 之一",
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
    set_f32(
        &mut echo,
        &p,
        "intensity",
        &mut sky.intensity,
        "天空亮度倍数",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "planet_radius",
        &mut sky.planet_radius,
        "行星半径",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "atmosphere_height",
        &mut sky.atmosphere_height,
        "大气壳厚度",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_rgb(
        &mut echo,
        &p,
        "rayleigh",
        &mut sky.rayleigh_scattering,
        "瑞利散射系数",
    )?;
    set_f32(
        &mut echo,
        &p,
        "rayleigh_scale_height",
        &mut sky.rayleigh_scale_height,
        "瑞利标高",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "mie",
        &mut sky.mie_scattering,
        "米氏散射系数",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "mie_scale_height",
        &mut sky.mie_scale_height,
        "米氏标高",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "mie_anisotropy",
        &mut sky.mie_anisotropy,
        "米氏各向异性 g",
        |v| v > -1.0 && v < 1.0,
        "须落在 (-1, 1)",
    )?;
    set_rgb(
        &mut echo,
        &p,
        "ozone",
        &mut sky.ozone_absorption,
        "臭氧吸收系数",
    )?;
    set_f32(
        &mut echo,
        &p,
        "ozone_center",
        &mut sky.ozone_center,
        "臭氧层中心高度",
        |v| v >= 0.0,
        "须为非负有限值",
    )?;
    set_f32(
        &mut echo,
        &p,
        "ozone_width",
        &mut sky.ozone_width,
        "臭氧层全宽",
        |v| v > 0.0,
        "须为正有限值",
    )?;
    set_rgb(
        &mut echo,
        &p,
        "ground_albedo",
        &mut sky.ground_albedo,
        "地面反照率",
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
        "sun_radius",
        &mut sky.sun_radius,
        "太阳角半径",
        |v| v > 0.0 && v < MAX_SUN_RADIUS,
        "须落在 (0, π/2)",
    )?;
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params(
            "无有效字段（enabled/intensity/rayleigh/mie/ozone/… 任一）",
        ));
    }
    Ok(json!({ "applied": echo, "current": current(&sky) }))
}

/// `ash_renderer/sky_stats`：当前天空参数只读回显。
#[system]
pub(crate) fn sky_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    Ok(current(res::<SkySettings>(world)?))
}

/// `ash_renderer/sky_uniform`：常量天空开关（9.2 自检专用）。独立方法——它带"清空
/// 语义"（关 = 回物理天空），与 `sky_set` 的"缺省字段不动"是两种更新形状。
/// 请求 `{"enabled": bool, "color": [r,g,b]?}`；开时 `sky_radiance` 直接返回
/// `color`（忽略大气/太阳/`intensity`），于是 SH-L2 的 `E(n) = π·color` 是解析真值。
#[system]
pub(crate) fn sky_uniform(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> =
        parse_params(params, "params 缺失：需要 enabled（可选 color）")?;
    let mut sky = res_mut::<SkySettings>(world)?;
    let mut echo = json!({});
    if let Some(v) = p.get("color") {
        let arr = v
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| invalid_params("color 需要长度 3 的数组"))?;
        let mut rgb = [0.0f32; 3];
        for (i, c) in arr.iter().enumerate() {
            let f = c
                .as_f64()
                .ok_or_else(|| invalid_params(format!("color[{i}] 非数值")))?
                as f32;
            if !f.is_finite() || f < 0.0 {
                return Err(invalid_params(format!("color[{i}] 须为非负有限值")));
            }
            rgb[i] = f;
        }
        sky.uniform_radiance = rgb;
        echo["color"] = json!(rgb);
    }
    if let Some(v) = p.get("enabled") {
        let enabled = v
            .as_bool()
            .ok_or_else(|| invalid_params("enabled 非布尔"))?;
        sky.uniform_enabled = enabled;
        echo["enabled"] = json!(enabled);
    }
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params("无有效字段（enabled/color 任一）"));
    }
    Ok(json!({
        "applied": echo,
        "uniform_enabled": sky.uniform_enabled,
        "uniform_radiance": sky.uniform_radiance,
    }))
}

/// 天空参数现值（`sky_set` 回显与 `sky_stats` 共用）。
fn current(sky: &SkySettings) -> Value {
    json!({
        "enabled": sky.enabled,
        "intensity": sky.intensity,
        "planet_radius": sky.planet_radius,
        "atmosphere_height": sky.atmosphere_height,
        "rayleigh": sky.rayleigh_scattering,
        "rayleigh_scale_height": sky.rayleigh_scale_height,
        "mie": sky.mie_scattering,
        "mie_scale_height": sky.mie_scale_height,
        "mie_anisotropy": sky.mie_anisotropy,
        "ozone": sky.ozone_absorption,
        "ozone_center": sky.ozone_center,
        "ozone_width": sky.ozone_width,
        "ground_albedo": sky.ground_albedo,
        "sun_gain": sky.sun_gain,
        "sun_radius": sky.sun_radius,
    })
}
