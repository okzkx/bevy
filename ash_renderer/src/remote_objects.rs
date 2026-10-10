//! BRP 对象生命周期方法族（4.1.5）：测试驱动与 AI 通道的实体级变更入口。
//!
//! 分工（按"不重复造官方已有"的钉子）：**despawn/摘组件走官方内置**
//! `world.despawn_entity` / `world.remove_components`（反射通道按组件名摘除，
//! 不需要构造组件值——brp.py 薄壳封装）；**spawn/replace 自研**——官方
//! `world.spawn_entity` 经反射构造组件值，表达不了"引用既有资产的柄"
//!（Handle 无法经反射指到现有资产身份），克隆/拷柄是场景语义操作。统计读数
//! `increment_stats` 自研：账本是普通 Rust 结构（BTreeMap + 柄），反射序列化
//! 不可读，定制 JSON 才能按判定线口径报数。
//!
//! 机制与边界：handler 是 Bevy system，跑在 `RemoteLast`（Last 之后）按帧节拍
//! 执行——写操作下一帧生效（spawn/despawn 在次帧画面可见，实测钉明）；操作
//! 本体在 [`crate::scene::mechanism::object_ops`]（与将来 egui 面板共用的可复用
//! 实现），本模块只是协议薄壳。安全边界：默认只绑 loopback、无鉴权，仅限本机。

use bevy::{pbr::StandardMaterial, prelude::*, remote::{error_codes, BrpError, BrpResult}};
use serde_json::{json, Value};

use crate::scene::mechanism::{
    replace_handles as apply_handle_swap, spawn_primitive_clone, HandleSwapReadback,
    InstanceLedger, ObjectOpError,
};

use super::remote::invalid_params;

use ash_macros::system;

/// `ash_renderer/spawn_primitive` 的请求参数。
#[derive(serde::Deserialize)]
struct SpawnPrimitiveParams {
    /// 克隆源（必须是可渲染 primitive）。
    source: Entity,
    /// 新实体相对源的平移偏移，缺省 [0,0,0]。
    #[serde(default)]
    offset: Option<[f32; 3]>,
    /// Name 后缀，缺省 "（副本）"。
    #[serde(default)]
    name_suffix: Option<String>,
}

/// 把操作层错误映射到协议错误码（ObjectOpError 是 Tier② 语义的载体，这里
/// 只做翻译，不吞——原 message 原样带出）。
fn object_op_error(err: ObjectOpError) -> BrpError {
    match err {
        ObjectOpError::EntityMissing(e) => BrpError::entity_not_found(e),
        ObjectOpError::SourceNotPrimitive(e) => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("源实体 {e:?} 不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）"),
            data: None,
        },
        ObjectOpError::ComponentMissing(name, entity) => {
            BrpError::component_not_present(name, entity)
        }
        ObjectOpError::NothingToDo => invalid_params("至少一项变更（mesh_from / material_from）"),
    }
}

/// `ash_renderer/spawn_primitive`：克隆源 primitive（共享资产柄 + 平移偏移）。
/// 账本经 Changed 通道自动入行，资产已驻留 → 无上传，画面次帧多出一个副本。
/// 返回新实体号与柄身份（写后读对账用）。
#[system]
pub(crate) fn spawn_primitive(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 source"))?;
    let p: SpawnPrimitiveParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let offset = p.offset.map(Vec3::from_array).unwrap_or(Vec3::ZERO);
    let entity = spawn_primitive_clone(world, p.source, offset, p.name_suffix.as_deref().unwrap_or("（副本）"))
        .map_err(object_op_error)?;
    // 刚克隆成功，组件必在；unwrap 的是不变式而非外部输入（Tier② 语义）
    let mesh = world
        .get::<Mesh3d>(entity)
        .map(|m| m.0.id().to_string())
        .unwrap_or_default();
    let material = world
        .get::<MeshMaterial3d<StandardMaterial>>(entity)
        .map(|m| m.0.id().to_string())
        .unwrap_or_default();
    Ok(json!({
        "entity": entity.to_string(),
        "mesh": mesh,
        "material": material,
    }))
}

/// `ash_renderer/replace_handles` 的请求参数：逐项可选（缺省 = 不动），
/// 至少一项。源实体须是可渲染 primitive（柄从它身上拷）。
#[derive(serde::Deserialize)]
struct ReplaceHandlesParams {
    entity: Entity,
    #[serde(default)]
    mesh_from: Option<Entity>,
    #[serde(default)]
    material_from: Option<Entity>,
}

/// `ash_renderer/replace_handles`：换柄——把源实体的 mesh/material 柄拷到目标。
/// 账本经 Changed 通道换绑，旧资产引用计数下降（CPU 容器自动），GPU 侧回收
/// 时机归 4.4，本段只主张"不再画旧对象"。返回换后柄身份读回。
#[system]
pub(crate) fn replace_handles(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 entity 与至少一项 from"))?;
    let p: ReplaceHandlesParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let HandleSwapReadback { mesh, material } =
        apply_handle_swap(world, p.entity, p.mesh_from, p.material_from).map_err(object_op_error)?;
    Ok(json!({
        "entity": p.entity.to_string(),
        "mesh": mesh.map(|id| id.to_string()),
        "material": material.map(|id| id.to_string()),
    }))
}

/// `ash_renderer/increment_stats`：账本与增量计量读数（只读，判定线证据通道）。
/// 处理数/扫描数分列——"零上传"不说成"零 CPU 每对象成本"。
#[system]
pub(crate) fn increment_stats(In(_params): In<Option<Value>>, ledger: Res<InstanceLedger>) -> BrpResult {
    let s = ledger.stats;
    Ok(json!({
        "rows": ledger.len(),
        "discovery": {
            "scan_candidates": s.scan_candidates,
            "added_rows": s.added_rows,
            "removed_rows": s.removed_rows,
            "rebound_rows": s.rebound_rows,
            "transform_updates": s.transform_updates,
            "processed_rows": s.processed_rows,
            "added_rows_total": s.added_rows_total,
            "removed_rows_total": s.removed_rows_total,
            "rebound_rows_total": s.rebound_rows_total,
            "transform_updates_total": s.transform_updates_total,
        },
        "upload": {
            "dirty_rows": s.dirty_rows,
            "batches_total": s.upload_batches_total,
            "bytes_total": s.upload_bytes_total,
            "last_batch_bytes": s.last_batch_bytes,
        },
    }))
}

// `ash_renderer/despawn`/`ash_renderer/remove` 不在本模块：前者=官方
// `world.despawn_entity`（0.20-dev 默认递归删 `Children` 子树）、后者=官方
// `world.remove_components`，反射通道按组件名操作即可（无值构造），brp.py
// 直接封装官方方法名——object_ops 的 despawn_tree/remove_component 封装留给
// 将来 egui 面板复用（组件白名单见 `RemovableComponent`）。
