//! 8.5 BRP SMAA 方法族：开关（smaa_set）。薄壳——参数直写 SmaaSettings。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::SmaaSettings;

use super::{invalid_params, BrpResult};

/// `ash_renderer/smaa_set`：SMAA 开关（A/B 判定线驱动面）。
#[system]
pub(crate) fn smaa_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 enabled"))?;
    let p: serde_json::Map<String, Value> =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let enabled = p
        .get("enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| invalid_params("enabled 非布尔"))?;
    let old = world.resource::<SmaaSettings>().enabled;
    world.resource_mut::<SmaaSettings>().enabled = enabled;
    Ok(json!({ "enabled": enabled, "previous": old }))
}
