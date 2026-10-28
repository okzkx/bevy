//! 布局契约报表方法族：`layout_contract`——宿主全部描述符集布局声明与管线配方
//! 的机器可读清单（只读）。数据源是编译期静态登记表
//! [`crate::render::core::binding::layout_registry`]，与 shader ABI 对账同源，
//! 免翻代码即可对账 set/binding 表（取代手写文档对账）。
//!
//! 红线边界照 perf 族：只读，读数不回灌绘制决策。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use super::BrpResult;
use crate::render::{ALL_PIPELINES, ALL_SETS};

/// `ash_renderer/layout_contract`：布局契约报表（只读）。
/// `sets` = 13 份 set 声明的 binding 表；`pipelines` = 22 条配方（shader/入口/
/// set 序列/push）。
#[system]
pub(crate) fn layout_contract(In(_params): In<Option<Value>>) -> BrpResult {
    let sets: Vec<Value> = ALL_SETS
        .iter()
        .map(|desc| {
            json!({
                "name": desc.name,
                "flags": format!("{:?}", desc.flags),
                "bindings": desc.bindings.iter().map(|b| json!({
                    "binding": b.binding,
                    "kind": format!("{:?}", b.kind),
                    "count": b.count,
                    "stages": format!("{:?}", b.stages),
                    "flags": format!("{:?}", b.flags),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let pipelines: Vec<Value> = ALL_PIPELINES
        .iter()
        .map(|desc| {
            json!({
                "label": desc.label,
                "shader": desc.shader.name,
                "entries": desc.entries.iter().map(|e| e.to_str().unwrap_or("<非 UTF-8>")).collect::<Vec<_>>(),
                "sets": desc.sets.iter().map(|s| s.name).collect::<Vec<_>>(),
                "push": desc.push.map(|p| json!({
                    "stages": format!("{:?}", p.stages),
                    "size": p.size,
                })),
                "color_formats": desc.color_formats.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>(),
                "depth_format": desc.depth_format.map(|f| format!("{f:?}")),
            })
        })
        .collect();
    Ok(json!({ "sets": sets, "pipelines": pipelines }))
}
