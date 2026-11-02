//! BRP 对象生命周期方法族：实体级变更入口与账本读数。
//!
//! 分工：despawn/摘组件走官方内置 `world.despawn_entity` / `world.remove_components`
//!（反射按组件名操作，无需构造组件值；官方 despawn 递归删 `Children` 子树）；
//! spawn/换柄自研——官方 `world.spawn_entity` 经反射构造组件值，表达不了"引用
//! 既有资产的柄"。`increment_stats` 自研：账本是普通 Rust 结构，反射不可读，定制
//! JSON 才能按验收口径报数。
//!
//! 写操作下一帧生效（handler 跑在 `RemoteLast`）；操作本体在
//! [`crate::scene::spawn_primitive_clone`] 等封装（egui 面板共用），
//! 本模块只是协议薄壳。

use bevy::{pbr::StandardMaterial, prelude::*, remote::{BrpError, BrpResult}};
use serde_json::{json, Value};

use crate::{
    scene::{
        replace_handles as apply_handle_swap, spawn_primitive_clone, AssetReadiness,
        AssetRetirements, HandleSwapReadback, InstanceLedger, ObjectOpError,
    },
    render::{BindlessTextureTable, ImageCache, InstanceTables, MeshPool, OBJECT_TABLE_CAPACITY},
};

use super::params::{invalid_params, parse_params};

use ash_macros::system;

/// `ash_renderer/spawn_primitive` 的请求参数。
#[derive(serde::Deserialize)]
struct SpawnPrimitiveParams {
    /// 克隆源（必须是可渲染 primitive）。
    source: Entity,
    /// 新实体相对源的平移偏移，缺省 `[0,0,0]`。
    #[serde(default)]
    offset: Option<[f32; 3]>,
    /// Name 后缀，缺省 "（副本）"。
    #[serde(default)]
    name_suffix: Option<String>,
}

/// 把操作层错误映射到协议错误码（只翻译不吞，原 message 原样带出）。
/// assets/generate/shapes 方法族共用。
pub(crate) fn object_op_error(err: ObjectOpError) -> BrpError {
    match err {
        ObjectOpError::EntityMissing(e) => BrpError::entity_not_found(e),
        ObjectOpError::SourceNotPrimitive(e) => BrpError::component_error(format!(
            "源实体 {e:?} 不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）"
        )),
        ObjectOpError::ComponentMissing(name, entity) => {
            BrpError::component_not_present(name, entity)
        }
        ObjectOpError::NothingToDo => invalid_params("至少一项变更（mesh_from / material_from）"),
        ObjectOpError::MaterialAssetMissing(e) => {
            BrpError::component_error(format!("实体 {e:?} 的材质资产不在容器中，槽写拒绝"))
        }
        ObjectOpError::InvalidAssetPath(what) => invalid_params(format!(
            "资产路径解析失败（坏标签/坏 source 语法）：{what}"
        )),
        ObjectOpError::InvalidParams(what) => invalid_params(what),
        ObjectOpError::ChunkMissing(id) => invalid_params(format!(
            "chunk {id} 不在场（ash_renderer/chunk_census 可查在场块）"
        )),
    }
}

/// 实体材质柄身份的字符串读回；刚 spawn/换柄成功组件必在，查无按空串读回不报错。
pub(crate) fn material_id_string(world: &World, entity: Entity) -> String {
    world
        .get::<MeshMaterial3d<StandardMaterial>>(entity)
        .map(|m| m.0.id().to_string())
        .unwrap_or_default()
}

/// `ash_renderer/spawn_primitive`：克隆源 primitive（共享资产柄 + 平移偏移）。
/// 账本经 Changed 通道自动入行，资产已驻留 → 无上传，画面次帧多出一个副本。
/// 返回新实体号与柄身份（写后读对账用）。
#[system]
pub(crate) fn spawn_primitive(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SpawnPrimitiveParams = parse_params(params, "params 缺失：需要 source")?;
    let offset = p.offset.map(Vec3::from_array).unwrap_or(Vec3::ZERO);
    let entity = spawn_primitive_clone(world, p.source, offset, p.name_suffix.as_deref().unwrap_or("（副本）"))
        .map_err(object_op_error)?;
    // 刚克隆成功，组件必在；查无按空串读回，不报错
    let mesh = world
        .get::<Mesh3d>(entity)
        .map(|m| m.0.id().to_string())
        .unwrap_or_default();
    Ok(json!({
        "entity": entity.to_string(),
        "mesh": mesh,
        "material": material_id_string(world, entity),
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
/// 账本经 Changed 通道换绑，旧资产引用计数下降（CPU 容器自动），GPU 侧
/// 回收由回收链另管，本方法只保证"不画旧对象"。返回换后柄身份读回。
#[system]
pub(crate) fn replace_handles(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ReplaceHandlesParams = parse_params(params, "params 缺失：需要 entity 与至少一项 from")?;
    let HandleSwapReadback { mesh, material } =
        apply_handle_swap(world, p.entity, p.mesh_from, p.material_from).map_err(object_op_error)?;
    Ok(json!({
        "entity": p.entity.to_string(),
        "mesh": mesh.map(|id| id.to_string()),
        "material": material.map(|id| id.to_string()),
    }))
}

/// `ash_renderer/increment_stats`：账本与增量计量读数（只读，验收证据通道）。
/// 处理数/扫描数分列——"零上传"不说成"零 CPU 每对象成本"。readiness 段
/// 报异步就绪账（pending/等待/到货/失败/取消分列）；recycle 段报卸载回收
/// 账（事件/出账/驻留存量/停尸房/free list 分列——"存量回基线"的读数口）。
#[expect(
    clippy::too_many_arguments,
    reason = "BRP 只读读数口的参数表即数据源清单:账本/就绪/回收/池/贴图缓存/表/常驻表逐项只读,拆参数束过度设计"
)]
#[system]
pub(crate) fn increment_stats(
    In(_params): In<Option<Value>>,
    ledger: Res<InstanceLedger>,
    readiness: Res<AssetReadiness>,
    retirements: Res<AssetRetirements>,
    pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
    tables: Res<BindlessTextureTable>,
    instance_tables: Res<InstanceTables>,
) -> BrpResult {
    let s = ledger.stats;
    let r = readiness.stats;
    let c = retirements.stats;
    let p = pool.recycle_ledger();
    let ((vertex_used_peak, vertex_capacity), _) = pool.usage();
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
        "asset_events": {
            "frame": s.asset_events,
            "frame_dirty_rows": s.asset_dirty_rows,
            "total": s.asset_events_total,
            "invalidations_total": s.asset_invalidations_total,
            "invalidations_current": s.asset_invalidations_current,
        },
        "gpu_tables": {
            "object_slots": s.object_slots,
            "object_capacity": instance_tables.capacity_objects(),
            "object_capacity_const": OBJECT_TABLE_CAPACITY,
            "object_graveyard": instance_tables.graveyard_count(),
            "object_retired_total": instance_tables.slots_retired_total(),
            "material_slots": s.material_slots,
            "material_capacity": instance_tables.capacity_materials(),
            // 图形 push 已退役（firstInstance 携带槽号）——场景绘制 push 字节
            // 恒 0；字段保留供压测脚本对照
            "push_bytes_per_draw": 0,
        },
        "readiness": {
            "pending_meshes": r.pending_meshes,
            "pending_images": r.pending_images,
            "wait_frames_total": r.wait_frames_total,
            "last_arrival_wait": r.last_arrival_wait,
            "arrivals_total": r.arrivals_total,
            "failures_total": r.failures_total,
            "failures_frame": r.failures_frame,
            "rejects_total": r.rejects_total,
            "cancelled_total": r.cancelled_total,
            "failed_rows_cleared_total": r.failed_rows_cleared_total,
        },
        "recycle": {
            "removed_meshes_total": c.removed_meshes_total,
            "removed_images_total": c.removed_images_total,
            "unused_events_total": c.unused_events_total,
            "resident_reclaimed_total": c.resident_reclaimed_total,
            "slots_reclaimed_total": c.slots_reclaimed_total,
            "pool_resident_count": pool.resident_count(),
            "pool_resident_vertex_bytes": p.resident_vertex_bytes,
            "pool_resident_index_bytes": p.resident_index_bytes,
            "pool_used_peak_vertex": vertex_used_peak,
            "pool_capacity_vertex": vertex_capacity,
            "pool_graveyard_groups": p.graveyard_groups,
            "pool_free_ranges": p.free_ranges,
            "pool_free_vertex_bytes": p.free_vertex_bytes,
            "pool_free_index_bytes": p.free_index_bytes,
            "image_resident_count": image_cache.resident_count(),
            "image_graveyard": image_cache.graveyard_len(),
            "texture_slots_used": tables.used_texture_slots(),
            "sampler_slots_used": tables.used_sampler_slots(),
            "tables_retired_slots": tables.retired_slot_count(),
        },
    }))
}
