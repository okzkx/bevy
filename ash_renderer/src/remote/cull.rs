//! 剔除读数与开关方法族（5.2）：BRP 侧的验证通道——`cull_stats` 做 GPU 标记
//! vs CPU 参照的逐实体对照（判定线 1 的直接证据），`cull_set_enabled` 写剔除
//! 开关（数据层 A/B 的远程驱动，egui checkbox 与此同源）。
//!
//! 红线边界（判定线 2）：两个方法都只读标记/只写开关，**不提供任何"按标记
//! 改场景"的通道**——诊断计数与对照读数永远不回灌绘制决策。
//!
//! 读数陈旧性：GPU 标记是帧槽直读（`marks_frame` 槽两帧前 compute 的输出），
//! CPU 参照按 handler 时刻的相机/账本现算——对照前提 = 相机与变换自标记
//! 产生以来未变（测试惯例：set_transform/set_camera 后等 ≥3 帧再查）。

use bevy::{
    camera::{Camera, Projection},
    prelude::*,
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    common::error::VulkanError,
    scene::InstanceLedger,
    vulkan::{
        conservative_world_aabb, aabb_visible, frustum_planes, CullDiagnostics, CullSettings,
        InstanceTables, MARK_SIZE, MeshPool, OBJECT_TABLE_CAPACITY,
    },
};

use super::{error_codes, invalid_params, BrpError, BrpResult};

/// `ash_renderer/cull_stats`：剔除读数对照（只读）。返回开关状态、dispatch
/// 上界、GPU/CPU 两列逐实体可见性与不一致清单——判定线 1 的证据形态。
///
/// # Errors
/// 参数不合法 / 相机缺席 / 标记环读失败（host-visible 契约被打破，Tier② 级）。
#[system]
pub(crate) fn cull_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // —— 相机 view_proj（与 draw_frame 同一乘序；缺席 = 场景装配问题，报错）
    let view_proj = {
        let mut cameras =
            world.query_filtered::<(&Projection, &GlobalTransform), With<Camera>>();
        match cameras.single(world) {
            Ok((projection, global)) => projection.get_clip_from_view() * global.affine().inverse(),
            Err(_) => {
                return Err(BrpError {
                    code: error_codes::COMPONENT_ERROR,
                    message: "World 里没有相机——CPU 参照无法提取视锥".to_owned(),
                    data: None,
                });
            }
        }
    };
    let planes = frustum_planes(&view_proj);
    // 读数快照先行取值（避免与下方标记环可变借用交叠）
    let (marks_frame, bound, enabled) = {
        let diag = world.resource::<CullDiagnostics>();
        let settings = world.resource::<CullSettings>();
        (diag.marks_frame, diag.bound, settings.enabled)
    };

    // —— CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒 → 六平面求交。
    // 行先收拢（账本/pool 只读借用不与标记环写读并存）
    let pool = world.resource::<MeshPool>();
    let mut reference: Vec<(Entity, u32, bool)> = Vec::new();
    for (entity, row) in world.resource::<InstanceLedger>().rows() {
        let Some(slot) = row.instance_slot else {
            continue; // 未驻留行无标记可言（DrawList 同款暂缓语义）
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let (wmin, wmax) = conservative_world_aabb(
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
        );
        let visible = aabb_visible(&planes, wmin.into(), wmax.into());
        reference.push((entity, slot, visible));
    }

    // —— GPU 标记：marks_frame 槽直读（fence 纪律内，handler 不等帧）
    let frame = marks_frame;
    let mut gpu = vec![0u8; OBJECT_TABLE_CAPACITY as usize * MARK_SIZE as usize];
    world
        .resource_mut::<InstanceTables>()
        .mark_ring(frame)
        .read(0, &mut gpu)
        .map_err(|e: VulkanError| BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("标记环读失败: {e}"),
            data: None,
        })?;
    let mark_at = |slot: u32| {
        let o = slot as usize * 4;
        u32::from_le_bytes(gpu[o..o + 4].try_into().expect("4B"))
    };

    let mut mismatches = Vec::new();
    let mut gpu_visible = 0u32;
    let mut cpu_visible = 0u32;
    let mut rows = Vec::with_capacity(reference.len());
    for (entity, slot, cpu) in reference {
        let mark = mark_at(slot);
        let gpu = mark == 1;
        if gpu {
            gpu_visible += 1;
        }
        if cpu {
            cpu_visible += 1;
        }
        if gpu != cpu {
            mismatches.push(json!({ "entity": format!("{entity:?}"), "slot": slot, "gpu": gpu, "cpu": cpu }));
        }
        rows.push(json!({
            "entity": format!("{entity:?}"),
            "slot": slot,
            "gpu": gpu,
            "cpu": cpu,
        }));
    }
    Ok(json!({
        "enabled": enabled,
        "bound": bound,
        "marks_frame": frame,
        "staleness_note": "GPU 标记 = marks_frame 槽两帧前 compute 输出；对照前提 = 相机与变换自标记产生以来未变",
        "gpu_visible": gpu_visible,
        "cpu_visible": cpu_visible,
        "rows_compared": rows.len(),
        "mismatches": mismatches,
        "rows": rows,
    }))
}

/// `ash_renderer/cull_set_enabled` 参数。
#[derive(serde::Deserialize)]
struct SetEnabledParams {
    enabled: bool,
}

/// `ash_renderer/cull_set_enabled`：写剔除开关（true = compute dispatch，
/// false = host 全可见填充）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn cull_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetEnabledParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|e| invalid_params(format!("enabled: {e}")))?;
    world.resource_mut::<CullSettings>().enabled = p.enabled;
    Ok(json!({ "enabled": p.enabled }))
}
