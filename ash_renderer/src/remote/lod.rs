//! LOD 读数与开关方法族：BRP 侧的验证通道——`lod_census` 做 GPU 档位 vs
//! CPU 参照的逐实例对照（选档正确性的直接证据 + 档位分布读数），
//! `lod_set_enabled`/`lod_set_params` 写开关与判据参数（数据层 A/B 的远程
//! 驱动，egui checkbox 与此同源）。
//!
//! 红线边界（照 cull 族同款）：三个方法都只读档位/只写开关与判据，**不提供
//! 任何"按档位改场景"的通道**——诊断读数永远不回灌绘制决策。
//!
//! 读数陈旧性与对照前提（照 cull_stats 惯例）：GPU 档位是帧槽直读（本帧槽
//! 两帧前 compute 的输出），CPU 参照按 handler 时刻的相机/账本现算——对照
//! 前提 = 相机与变换自标记产生以来未变（测试惯例：set_camera/set_transform
//! 后等 ≥3 帧再查）。迟滞同源：CPU 参照的 prev 取自同一档位环（GPU 用的
//! 就是它），公式同 [`lod_hysteresis_level`] 逐位同源——静态场景下对照逐
//! 实例一致，带内微动不构成 mismatch。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    common::error::VulkanError,
    scene::{InstanceLedger, LodGroups},
    render::{
        aabb_screen_rect, lod_hysteresis_level, lod_raw_level, InstanceTables, LOD_LEVEL_SIZE,
        LodDiagnostics, LodSettings, MAX_LOD_LEVELS, MeshPool,
    },
};

use super::{
    invalid_params,
    params::{brp_view_proj, clamp_rect_to_screen, parse_params},
    readback::u32_at,
    BrpError, BrpResult,
};

/// `ash_renderer/lod_census`：LOD 档位对照（只读）。返回开关状态、判据参数、
/// dispatch 上界、档位分布（GPU 两帧前读数）、逐实例 GPU/CPU 档位与不一致
/// 清单。
///
/// # Errors
/// 相机缺席 / 档位环读失败（host-visible 契约被打破，Tier② 级）。
#[system]
pub(crate) fn lod_census(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // 相机缺席 = 场景装配问题，报错
    let view_proj = brp_view_proj(world, "投屏参照无法计算")?;
    let (marks_frame, bound, enabled, threshold, hysteresis) = {
        let (diag, settings) = (
            world.resource::<LodDiagnostics>(),
            world.resource::<LodSettings>(),
        );
        (
            diag.marks_frame,
            diag.bound,
            settings.enabled,
            settings.threshold_px,
            settings.hysteresis,
        )
    };
    // —— CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒投屏（clamp 后最大边
    //    = 尺寸度量，与 shader 同式）→ 自然档位 + 迟滞裁决（prev = GPU 档位
    //    环本槽值，同源）。行先收拢（账本/pool 只读借用不与档位环写读并存）。
    let pool = world.resource::<MeshPool>();
    let lod_groups = world.resource::<LodGroups>();
    let swapchain_extent = {
        let swapchain = world.resource::<crate::render::Swapchain>();
        [
            swapchain.extent.width as f32,
            swapchain.extent.height as f32,
        ]
    };
    let mut reference: Vec<(Entity, u32, u32, f32, bool)> = Vec::new();
    for (entity, row) in world.resource::<InstanceLedger>().rows() {
        let Some(slot) = row.instance_slot else {
            continue;
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let in_group = lod_groups.levels_of(row.mesh.id()).is_some();
        let Some((px_min, px_max, _)) = aabb_screen_rect(
            &view_proj,
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
            swapchain_extent,
        ) else {
            // 角点投影 w≤0（相机平面背后）——shader 同款恒最细档
            reference.push((entity, slot, 0, 0.0, in_group));
            continue;
        };
        // 坑：必须与 lod_select.wgsl 同口径先钳进屏再量尺寸，否则离屏/半离屏
        // 行 CPU 判细档、GPU 判粗档（完全离屏 → 0 → 最粗档）
        let (px_min, px_max) = clamp_rect_to_screen(px_min, px_max, swapchain_extent);
        let size = (px_max.x - px_min.x).max(px_max.y - px_min.y);
        let raw = lod_raw_level(size, threshold, MAX_LOD_LEVELS);
        reference.push((entity, slot, raw, size, in_group));
    }

    // —— GPU 档位环：marks_frame 槽直读（fence 纪律内，handler 不等帧）
    let frame = marks_frame;
    let mut gpu_bytes = vec![0u8; level_bytes_for_bound(bound)];
    world
        .resource_mut::<InstanceTables>()
        .lod_level_ring(frame)
        .read(0, &mut gpu_bytes)
        .map_err(|e: VulkanError| BrpError::internal(format!("档位环读失败: {e}")))?;

    let mut mismatches = Vec::new();
    let mut counts = [0u32; MAX_LOD_LEVELS as usize];
    let mut rows = Vec::with_capacity(reference.len());
    for (entity, slot, raw, size, in_group) in reference {
        let gpu = u32_at(&gpu_bytes, slot as usize * LOD_LEVEL_SIZE as usize)?.min(MAX_LOD_LEVELS - 1);
        counts[gpu as usize] += 1;
        // CPU 参照 = 同一公式 + 同一迟滞状态（prev = GPU 档位环本槽值——
        // 与 GPU 下一次 dispatch 将读的迟滞输入同一来源；静止场景两者
        // 收敛一致，即对照口径）
        let cpu = lod_hysteresis_level(raw, gpu, size, threshold, hysteresis, MAX_LOD_LEVELS);
        if gpu != cpu {
            mismatches.push(json!({
                "entity": format!("{entity:?}"), "slot": slot,
                "gpu": gpu, "cpu": cpu, "size_px": size,
            }));
        }
        rows.push(json!({
            "entity": format!("{entity:?}"), "slot": slot,
            "gpu": gpu, "cpu": cpu, "raw": raw,
            "size_px": size, "in_group": in_group,
        }));
    }
    Ok(json!({
        "enabled": enabled,
        "threshold_px": threshold,
        "hysteresis": hysteresis,
        "bound": bound,
        "marks_frame": frame,
        "staleness_note": "GPU 档位 = marks_frame 槽两帧前 compute 输出；对照前提 = 相机与变换自标记产生以来未变（静止 ≥3 帧）",
        "level_counts": counts,
        "rows_compared": rows.len(),
        "mismatches": mismatches,
        "rows": rows,
    }))
}

/// 档位环读数容量（按 dispatch 上界读 vs 全容量读的折中：诊断读数只关心
/// 在场行，但环按容量直址——读 bound 覆盖段即可，`bound` 取自诊断快照）。
fn level_bytes_for_bound(bound: u32) -> usize {
    bound as usize * LOD_LEVEL_SIZE as usize
}

/// `ash_renderer/lod_set_enabled` 参数。
#[derive(serde::Deserialize)]
struct SetEnabledParams {
    enabled: bool,
}

/// `ash_renderer/lod_set_enabled`：写 LOD 开关（true = 选档 dispatch，
/// false = host 填档位环 0 最细档——数据层 A/B 的远程驱动，egui checkbox
/// 与此同源）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn lod_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetEnabledParams = parse_params(params, "params 缺失：需要 enabled")?;
    world.resource_mut::<LodSettings>().enabled = p.enabled;
    Ok(json!({ "enabled": p.enabled }))
}

/// `ash_renderer/lod_set_params` 参数：逐项可选（缺省 = 不动）。
#[derive(serde::Deserialize)]
struct SetParams {
    #[serde(default)]
    threshold_px: Option<f32>,
    #[serde(default)]
    hysteresis: Option<f32>,
}

/// `ash_renderer/lod_set_params`：写判据参数（阈值/迟滞，随 96B push 每帧进
/// compute——A/B 与灵敏度扫描的远程驱动）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错/数值非法（threshold ≤ 0 或 hysteresis 出 [0,1)）。
#[system]
pub(crate) fn lod_set_params(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetParams =
        parse_params(params, "params 缺失：需要对象（threshold_px/hysteresis 可选）")?;
    let mut settings = world.resource_mut::<LodSettings>();
    if let Some(threshold) = p.threshold_px {
        if threshold <= 0.0 || threshold.is_nan() {
            return Err(invalid_params(format!(
                "threshold_px {threshold} 非法（须 > 0）"
            )));
        }
        settings.threshold_px = threshold;
    }
    if let Some(hysteresis) = p.hysteresis {
        if !(0.0..1.0).contains(&hysteresis) {
            return Err(invalid_params(format!(
                "hysteresis {hysteresis} 非法（须 ∈ [0,1)）"
            )));
        }
        settings.hysteresis = hysteresis;
    }
    Ok(json!({
        "threshold_px": settings.threshold_px,
        "hysteresis": settings.hysteresis,
        "enabled": settings.enabled,
    }))
}
