//! 画面设置方法族：`bloom_set`（Bloom/tonemap 开关与参数）、`smaa_set`（SMAA
//! 开关）、`set_render_mode`（着色调试模式）。薄壳——参数直写对应设置资源，帧侧
//! 每帧照资源重写参数 UBO / 选管线。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{BloomSettings, RenderMode, SmaaSettings, TonemapCurve};

use crate::remote::common::params::{invalid_params, parse_enabled, parse_params, res_mut};

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
