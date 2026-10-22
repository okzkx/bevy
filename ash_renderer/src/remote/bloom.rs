//! 8.4 BRP Bloom 方法族：开关与参数（bloom_set）。薄壳——参数直写
//! [`BloomSettings`] 资源，帧侧参数 UBO 由 draw_frame 每帧照资源重写。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::BloomSettings;

use super::{invalid_params, BrpResult};

/// `ash_renderer/bloom_set`：Bloom/tonemap 开关与强度。
/// 请求 `{"enabled": bool, "strength": f32?, "tonemap": bool?}`——缺省字段不动。
#[system]
pub(crate) fn bloom_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 enabled/strength/tonemap 之一"))?;
    let p: serde_json::Map<String, Value> =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let mut settings = world.resource_mut::<BloomSettings>();
    let mut echo = json!({});
    if let Some(v) = p.get("enabled") {
        let enabled = v.as_bool().ok_or_else(|| invalid_params("enabled 非布尔"))?;
        settings.enabled = enabled;
        echo["enabled"] = json!(enabled);
    }
    if let Some(v) = p.get("strength") {
        let strength = v.as_f64().ok_or_else(|| invalid_params("strength 非数值"))?;
        settings.strength = strength as f32;
        echo["strength"] = json!(strength);
    }
    if let Some(v) = p.get("tonemap") {
        let tonemap = v.as_bool().ok_or_else(|| invalid_params("tonemap 非布尔"))?;
        settings.tonemap = tonemap;
        echo["tonemap"] = json!(tonemap);
    }
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params("无有效字段（enabled/strength/tonemap 任一）"));
    }
    Ok(json!({ "applied": echo, "current": {
        "enabled": settings.enabled,
        "strength": settings.strength,
        "tonemap": settings.tonemap,
    }}))
}
