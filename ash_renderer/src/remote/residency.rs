//! 驻留管理器读数与设置方法族（6.3.5）：BRP 侧的验证通道——
//! `residency_stats` 做预算/实占/请求/淘汰的聚合读数（判定线 1 的证据形态）
//! 加逐资产清单（驻留态/距离/引用行数），`residency_set` 写开关/预算/请求
//! 半径（数据层 A/B 的远程驱动）。
//!
//! 红线边界（照 cull/lod 族同款）：两个方法都只读驻留账/只写设置，**不提供
//! 任何"按驻留账改场景"的通道**——诊断读数永远不回灌绘制决策。
//!
//! 读数时点：residency_tick 在 Last（先于上传），BRP handler 在 RemoteLast
//! ——读数 = 本帧 tick 之后的快照（比 GPU 标记新鲜，无 2 帧滞后问题）。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    scene::{AssetInvalidations, InstanceLedger, ResidencySettings, ResidencyStats},
    vulkan::MeshPool,
};

use super::{error_codes, invalid_params, BrpError, BrpResult};

/// `ash_renderer/residency_stats`：驻留读数（只读）。聚合段 = 预算/实占/
/// 请求/冷却/淘汰累计；资产段 = 逐 mesh 身份的驻留态与最近引用距离。
///
/// # Errors
/// 驻留资源缺席（禁渲染宿主）。
#[system]
pub(crate) fn residency_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let cam_pos = {
        let mut cameras = world.query_filtered::<&GlobalTransform, With<Camera>>();
        match cameras.single(world) {
            Ok(g) => g.translation(),
            Err(_) => {
                return Err(BrpError {
                    code: error_codes::COMPONENT_ERROR,
                    message: "World 里没有相机——距离读数无法计算".to_owned(),
                    data: None,
                });
            }
        }
    };
    let stats = world.resource::<ResidencyStats>().clone();
    let settings = world.resource::<ResidencySettings>();
    let pool = world.resource::<MeshPool>();
    let ledger = world.resource::<InstanceLedger>();
    // 逐身份聚合：驻留态 + 最近引用行距离 + 引用行数 + 待失效标记
    let mut assets: Vec<(AssetId<Mesh>, f32, usize, bool)> = Vec::new();
    {
        let mut seen = std::collections::HashMap::new();
        for row in ledger.rows().map(|(_, r)| r) {
            let id = row.mesh.id();
            let dist = (row.model.w_axis.truncate() - cam_pos).length();
            let e = seen.entry(id).or_insert((f32::INFINITY, 0usize));
            e.0 = e.0.min(dist);
            e.1 += 1;
        }
        assets.extend(seen.into_iter().map(|(id, (dist, rows))| (id, dist, rows, false)));
    }
    assets.sort_by(|a, b| a.1.total_cmp(&b.1));
    let invalidations = world.resource::<AssetInvalidations>();
    let rows: Vec<Value> = assets
        .into_iter()
        .map(|(id, dist, refs, _)| {
            let resident = pool.resident(id).is_some();
            json!({
                "mesh": id.to_string(),
                "resident": resident,
                "min_dist": dist,
                "rows": refs,
                "invalidating": invalidations.mesh_pending(id),
            })
        })
        .collect();
    Ok(json!({
        "enabled": stats.enabled,
        "budget_mesh_bytes": stats.budget_mesh_bytes,
        "used_mesh_bytes": stats.used_mesh_bytes,
        "resident_meshes": stats.resident_meshes,
        "resident_images": stats.resident_images,
        "requested": stats.requested,
        "cooling": stats.cooling,
        "ejected_total": stats.ejected_total,
        "requested_rows_total": stats.requested_rows_total,
        "ejected_last": stats.ejected_last,
        "requested_rows_last": stats.requested_rows_last,
        "over_budget": stats.over_budget,
        "rows": stats.rows,
        "settings": {
            "request_radius": settings.request_radius,
            "eject_cooldown_frames": settings.eject_cooldown_frames,
        },
        "assets": rows,
    }))
}

/// `ash_renderer/residency_set` 参数：逐项可选（缺省 = 不动）。
#[derive(serde::Deserialize)]
struct ResidencySetParams {
    #[serde(default)]
    enabled: Option<bool>,
    /// mesh 驻留预算（字节；0 = 不设限 = 全常驻口径）。
    #[serde(default)]
    budget_mesh_bytes: Option<u64>,
    #[serde(default)]
    request_radius: Option<f32>,
    #[serde(default)]
    eject_cooldown_frames: Option<u32>,
}

/// `ash_renderer/residency_set`：写驻留管理器设置（开关/预算/请求半径/
/// 冷却窗——数据层 A/B 的远程驱动）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错/数值非法（预算字节超 u32 位宽的显存口径不截断、半径负值）。
#[system]
pub(crate) fn residency_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ResidencySetParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|e| invalid_params(format!("params: {e}")))?;
    let mut settings = world.resource_mut::<ResidencySettings>();
    if let Some(enabled) = p.enabled {
        settings.enabled = enabled;
    }
    if let Some(budget) = p.budget_mesh_bytes {
        settings.budget_mesh_bytes = budget;
    }
    if let Some(radius) = p.request_radius {
        if radius <= 0.0 || radius.is_nan() {
            return Err(invalid_params(format!("request_radius {radius} 非法（须 > 0）")));
        }
        settings.request_radius = radius;
    }
    if let Some(cooldown) = p.eject_cooldown_frames {
        settings.eject_cooldown_frames = cooldown;
    }
    Ok(json!({
        "enabled": settings.enabled,
        "budget_mesh_bytes": settings.budget_mesh_bytes,
        "request_radius": settings.request_radius,
        "eject_cooldown_frames": settings.eject_cooldown_frames,
    }))
}
