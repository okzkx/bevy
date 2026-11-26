//! 画面设置方法族：`bloom_set`（Bloom/tonemap 开关与参数）、`smaa_set`（SMAA
//! 开关）、`set_render_mode`（着色调试模式）。薄壳——参数直写对应设置资源，帧侧
//! 每帧照资源重写参数 UBO / 选管线。

use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    BloomSettings, ExposureMode, ExposureSettings, ExposureTargets, FramePool, RenderMode,
    SmaaSettings, TonemapCurve,
};

use crate::remote::common::params::{
    internal_err, invalid_params, parse_enabled, parse_params, res, res_mut,
};

/// `ash_renderer/bloom_set`：Bloom/tonemap 开关、曲线与增益。
/// 请求 `{"enabled": bool, "strength": f32?, "tonemap": bool?,
/// "curve": "narkowicz"|"filament"?, "gain": f32?}`——缺省字段不动。
#[system]
pub(crate) fn bloom_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> = parse_params(
        params,
        "params 缺失：需要 enabled/strength/tonemap/curve/gain 之一",
    )?;
    let mut settings = res_mut::<BloomSettings>(world)?;
    let mut echo = json!({});
    if let Some(v) = p.get("enabled") {
        let enabled = v
            .as_bool()
            .ok_or_else(|| invalid_params("enabled 非布尔"))?;
        settings.enabled = enabled;
        echo["enabled"] = json!(enabled);
    }
    if let Some(v) = p.get("strength") {
        let strength = v
            .as_f64()
            .ok_or_else(|| invalid_params("strength 非数值"))?;
        settings.strength = strength as f32;
        echo["strength"] = json!(strength);
    }
    if let Some(v) = p.get("tonemap") {
        let tonemap = v
            .as_bool()
            .ok_or_else(|| invalid_params("tonemap 非布尔"))?;
        settings.tonemap = tonemap;
        echo["tonemap"] = json!(tonemap);
    }
    if let Some(v) = p.get("curve") {
        let curve = v
            .as_str()
            .ok_or_else(|| invalid_params("curve 需 \"narkowicz\"|\"filament\""))?;
        settings.curve = match curve {
            "narkowicz" => TonemapCurve::Narkowicz,
            "filament" => TonemapCurve::FilamentAces,
            other => return Err(invalid_params(format!("curve 未知值 '{other}'"))),
        };
        echo["curve"] = json!(curve);
    }
    if let Some(v) = p.get("gain") {
        let gain = v.as_f64().ok_or_else(|| invalid_params("gain 非数值"))?;
        if !(gain > 0.0 && gain.is_finite()) {
            return Err(invalid_params("gain 须为正有限值"));
        }
        settings.gain = gain as f32;
        echo["gain"] = json!(gain);
    }
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params(
            "无有效字段（enabled/strength/tonemap/curve/gain 任一）",
        ));
    }
    Ok(json!({ "applied": echo, "current": {
        "enabled": settings.enabled,
        "strength": settings.strength,
        "tonemap": settings.tonemap,
        "curve": match settings.curve {
            TonemapCurve::Narkowicz => "narkowicz",
            TonemapCurve::FilamentAces => "filament",
        },
        "gain": settings.gain,
    }}))
}

/// `ash_renderer/smaa_set`：SMAA 开关（A/B 对照驱动面），回写前值。
#[system]
pub(crate) fn smaa_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let enabled = parse_enabled(params)?;
    let mut settings = res_mut::<SmaaSettings>(world)?;
    let previous = std::mem::replace(&mut settings.enabled, enabled);
    Ok(json!({ "enabled": enabled, "previous": previous }))
}

/// `ash_renderer/exposure_set`：自动曝光（显示段独立模块）的开关与参数。
/// 请求 `{"mode": "manual"|"auto"?, "key": f32?, "min_scale": f32?,
/// "max_scale": f32?, "rate_brighten": f32?, "rate_darken": f32?}`——缺省字段不动。
/// `manual` 时倍数恒 1（A/B 对照、能量回归必须用这个模式，否则两边各自适应不同曝光）。
///
/// # Errors
/// 参数不合法（未知模式名、非正 key、上下限次序颠倒、负适应速率）。
#[system]
pub(crate) fn exposure_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> = parse_params(
        params,
        "params 缺失：需要 mode/key/min_scale/max_scale/rate_brighten/rate_darken 之一",
    )?;
    let mut settings = res_mut::<ExposureSettings>(world)?;
    let mut echo = json!({});
    if let Some(v) = p.get("mode") {
        let mode = v
            .as_str()
            .ok_or_else(|| invalid_params("mode 需 \"manual\"|\"auto\""))?;
        settings.mode = match mode {
            "manual" => ExposureMode::Manual,
            "auto" => ExposureMode::Auto,
            other => return Err(invalid_params(format!("mode 未知值 '{other}'"))),
        };
        echo["mode"] = json!(mode);
    }
    if let Some(v) = p.get("key") {
        let key = finite_scalar("key", v, false)?;
        settings.key = key;
        echo["key"] = json!(key);
    }
    if let Some(v) = p.get("min_scale") {
        settings.min_scale = finite_scalar("min_scale", v, false)?;
        echo["min_scale"] = json!(settings.min_scale);
    }
    if let Some(v) = p.get("max_scale") {
        settings.max_scale = finite_scalar("max_scale", v, false)?;
        echo["max_scale"] = json!(settings.max_scale);
    }
    if settings.min_scale > settings.max_scale {
        return Err(invalid_params("min_scale 不得大于 max_scale"));
    }
    if let Some(v) = p.get("rate_brighten") {
        settings.rate_brighten = finite_scalar("rate_brighten", v, true)?;
        echo["rate_brighten"] = json!(settings.rate_brighten);
    }
    if let Some(v) = p.get("rate_darken") {
        settings.rate_darken = finite_scalar("rate_darken", v, true)?;
        echo["rate_darken"] = json!(settings.rate_darken);
    }
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params(
            "无有效字段（mode/key/min_scale/max_scale/rate_* 任一）",
        ));
    }
    Ok(json!({
        "applied": echo,
        "current": exposure_json(&settings, None),
    }))
}

/// 校验标量参数：必须是有限值，`allow_zero` 决定 0 是否合法。
fn finite_scalar(name: &str, v: &Value, allow_zero: bool) -> Result<f32, BrpError> {
    let raw = v
        .as_f64()
        .ok_or_else(|| invalid_params(format!("{name} 非数值")))?;
    let ok = raw.is_finite() && if allow_zero { raw >= 0.0 } else { raw > 0.0 };
    if !ok {
        let sign = if allow_zero { "非负" } else { "正" };
        return Err(invalid_params(format!("{name} 须为{sign}有限值")));
    }
    Ok(raw as f32)
}

/// `ash_renderer/exposure_get`：读设置与最近一次提交槽的适应状态
///（`adapted_log2` / `scale` / `avg_log2` / `initialized`）。
///
/// # Errors
/// 资源缺失或状态缓冲读失败（设备已丢）。
#[system]
pub(crate) fn exposure_get(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let slot = res::<FramePool>(world)?.current_index();
    let state = res_mut::<ExposureTargets>(world)?
        .read_state(slot)
        .map_err(internal_err("曝光状态读失败"))?;
    let settings = res::<ExposureSettings>(world)?;
    Ok(exposure_json(settings, Some(state)))
}

/// 设置 + （可选）适应状态的统一 JSON 形态，`exposure_set`/`exposure_get` 共用。
fn exposure_json(settings: &ExposureSettings, state: Option<[f32; 4]>) -> Value {
    let mut out = json!({
        "mode": match settings.mode {
            ExposureMode::Manual => "manual",
            ExposureMode::Auto => "auto",
        },
        "key": settings.key,
        "min_scale": settings.min_scale,
        "max_scale": settings.max_scale,
        "rate_brighten": settings.rate_brighten,
        "rate_darken": settings.rate_darken,
    });
    if let Some(state) = state {
        out["adapted_log2"] = json!(state[0]);
        out["scale"] = json!(state[1]);
        out["avg_log2"] = json!(state[2]);
        out["initialized"] = json!(state[3] > 0.5);
    }
    out
}

/// `ash_renderer/set_render_mode`：着色模式切换（debug 面板 RenderMode 的 BRP
/// 通道）——unlit/normal/position 与 metallic/roughness/ao/reflectance/depth 是
/// G-buffer 逐通道调试视图（单通道灰度直出 + 深度线性化），画面异常时先切视图
/// 定位问题层。名称表唯一来源 [`RenderMode::from_name`]（"lambert" 为 filament
/// 的兼容别名）。
///
/// # Errors
/// 参数不合法（未知模式名）。
#[system]
pub(crate) fn set_render_mode(
    In(params): In<Option<Value>>,
    mut mode: ResMut<'_, RenderMode>,
) -> BrpResult {
    #[derive(serde::Deserialize)]
    struct P {
        mode: String,
    }
    let p: P = parse_params(params, "params 缺失：需要 mode")?;
    *mode = RenderMode::from_name(&p.mode)
        .ok_or_else(|| invalid_params(format!("未知模式 {}：{}", p.mode, RenderMode::NAME_HINT)))?;
    Ok(json!({ "mode": p.mode }))
}
