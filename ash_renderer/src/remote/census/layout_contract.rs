//! 布局契约报表方法族：`layout_contract`——宿主全部描述符集布局声明与管线配方
//! 的机器可读清单（只读）。数据源是编译期静态登记表
//! `render/layout_registry.rs`，与 shader ABI 对账同源，
//! 免翻代码即可对账 set/binding 表。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{ALL_PIPELINES, ALL_SETS};

/// `ash_renderer/layout_contract`：布局契约报表（只读）。`sets` = 各 set 声明的
/// binding 表；`pipelines` = 各管线配方（shader/入口/set 序列/push/附件格式）。
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
