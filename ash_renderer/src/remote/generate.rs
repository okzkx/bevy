//! 常驻大场景生成与 chunk 组织方法族（5.4）：`generate_scene` 程序化铺
//! 实例（批量克隆共享资产，账本链路自动入行，驻留资产零重传）；
//! `generate_chunks`/`chunk_census`/`chunk_despawn` 是 chunk 身份与整块
//! 增删（5.4.2）——判定线 1（复用/块边界可观察）与判定线 2（整块增删
//! 走账）的驱动面与证据通道。
//!
//! 分工：操作本体在 [`crate::scene::mechanism::scene_gen`]（BRP 与将来
//! egui 共用，object_ops 同款惯例），本模块只是协议薄壳。
//!
//! 红线边界：写操作是人/AI 的场景编辑意图（与 set_transform/spawn 同类），
//! 不存在"读数回灌绘制决策"的通道（draw.rs 红线口径不受影响）。

use bevy::{pbr::StandardMaterial, prelude::*};
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::mechanism::{
    chunk_census as chunk_census_rows, despawn_chunk, generate_chunks, generate_instances,
    ChunkGenParams, SceneGenParams,
};

use super::{invalid_params, objects::object_op_error, BrpResult};

/// `ash_renderer/generate_scene` 的请求参数：源实体缺省 = 场景里第一个
/// 可渲染 primitive（省去先查再铺的两步）；其余全显式——复现的前提是
/// 参数自足。
#[derive(serde::Deserialize)]
struct GenerateSceneParams {
    #[serde(default)]
    source: Option<Entity>,
    count: u32,
    columns: u32,
    spacing: f32,
    seed: u64,
    #[serde(default)]
    jitter: Option<f32>,
}

/// `ash_renderer/generate_scene`：批量克隆铺实例（共享资产柄 + 网格抖动
/// 布局，种子固定可复现）。账本次帧入行，驻留资产零上传；返回计数与
/// 首尾实体（入行对账锚点）。
#[system]
pub(crate) fn generate_scene(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value =
        params.ok_or_else(|| invalid_params("params 缺失：需要 count/columns/spacing/seed"))?;
    let p: GenerateSceneParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let source = match p.source {
        Some(s) => s,
        None => {
            let mut q = world
                .query_filtered::<Entity, (With<Mesh3d>, With<MeshMaterial3d<StandardMaterial>>)>();
            q.iter(world)
                .next()
                .ok_or_else(|| invalid_params("场景无可渲染 primitive，source 不能缺省"))?
        }
    };
    let readback = generate_instances(
        world,
        SceneGenParams {
            source,
            count: p.count,
            columns: p.columns,
            spacing: p.spacing,
            seed: p.seed,
            jitter: p.jitter.unwrap_or(0.0),
        },
    )
    .map_err(object_op_error)?;
    Ok(json!({
        "spawned": readback.spawned,
        "first": readback.first.to_string(),
        "last": readback.last.to_string(),
        "mesh": readback.mesh.to_string(),
        "material": readback.material.to_string(),
    }))
}

/// `ash_renderer/generate_chunks` 的请求参数：块内实例排布由生成器定
///（列数 = ceil(sqrt(instances_per_chunk))），调用方给块阵与间距。
#[derive(serde::Deserialize)]
struct GenerateChunksParams {
    #[serde(default)]
    source: Option<Entity>,
    chunk_grid: u32,
    instances_per_chunk: u32,
    spacing: f32,
    seed: u64,
    #[serde(default)]
    jitter: Option<f32>,
}

/// `ash_renderer/generate_chunks`：chunk 化批量生成——每轴 `chunk_grid` 块
///（空间网格，块边界即空间边界），每块一个根实体 + 子树实例。返回块/实例
/// 计数与首尾根实体。
#[system]
pub(crate) fn generate_chunks_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params
        .ok_or_else(|| invalid_params("params 缺失：需要 chunk_grid/instances_per_chunk/spacing/seed"))?;
    let p: GenerateChunksParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let source = match p.source {
        Some(s) => s,
        None => {
            let mut q = world
                .query_filtered::<Entity, (With<Mesh3d>, With<MeshMaterial3d<StandardMaterial>>)>();
            q.iter(world)
                .next()
                .ok_or_else(|| invalid_params("场景无可渲染 primitive，source 不能缺省"))?
        }
    };
    let readback = generate_chunks(
        world,
        ChunkGenParams {
            source,
            chunk_grid: p.chunk_grid,
            instances_per_chunk: p.instances_per_chunk,
            spacing: p.spacing,
            seed: p.seed,
            jitter: p.jitter.unwrap_or(0.0),
        },
    )
    .map_err(object_op_error)?;
    Ok(json!({
        "chunks": readback.chunks,
        "instances": readback.instances,
        "first_root": readback.first_root.to_string(),
        "last_root": readback.last_root.to_string(),
        "mesh": readback.mesh.to_string(),
        "material": readback.material.to_string(),
    }))
}

/// `ash_renderer/chunk_census`：chunk 盘点（只读）——在场块清单（id/根
/// 实体/原点/边长/成员数）与总量。复用对账口：total_members 应等于
/// increment_stats 的 rows 增量；块边界读数 = 逐块 origin/extent/members。
#[system]
pub(crate) fn chunk_census(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let rows = chunk_census_rows(world);
    let total_members: usize = rows.iter().map(|r| r.members).sum();
    let chunks: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "entity": r.entity.to_string(),
                "origin": [r.origin.x, r.origin.y, r.origin.z],
                "extent": r.extent,
                "members": r.members,
            })
        })
        .collect();
    Ok(json!({
        "chunk_count": chunks.len(),
        "total_members": total_members,
        "chunks": chunks,
    }))
}

/// `ash_renderer/chunk_despawn`：整块删除——按 id 找根，根连子树递归
/// despawn；逐 primitive 的 Removed 通道自动清账（4.1 整树清账的批量
/// 形态）。返回根实体（对账锚点：increment_stats 的 removed_rows 增量
/// 应等于该块 members）。
#[derive(serde::Deserialize)]
struct ChunkDespawnParams {
    id: u32,
}

#[system]
pub(crate) fn chunk_despawn(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 id"))?;
    let p: ChunkDespawnParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let root = despawn_chunk(world, p.id).map_err(object_op_error)?;
    Ok(json!({
        "chunk": p.id,
        "root": root.to_string(),
        "note": "removed_rows 对账看 increment_stats（次帧生效）",
    }))
}
