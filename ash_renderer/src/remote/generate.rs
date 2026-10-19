//! 常驻大场景生成方法族（5.4）：`generate_scene` 程序化铺实例——批量克隆
//! 共享资产，账本链路自动入行（4.1 三入口天然覆盖批量 spawn），驻留资产
//! 零重传。chunk 身份与整块增删方法族在 5.4.2 落地后进本模块。
//!
//! 分工：操作本体在 [`crate::scene::mechanism::scene_gen`]（BRP 与将来
//! egui 共用，object_ops 同款惯例），本模块只是协议薄壳。
//!
//! 红线边界：写操作是人/AI 的场景编辑意图（与 set_transform/spawn 同类），
//! 不存在"读数回灌绘制决策"的通道（draw.rs 红线口径不受影响）。

use bevy::{pbr::StandardMaterial, prelude::*};
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::mechanism::{generate_instances, SceneGenParams};

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
