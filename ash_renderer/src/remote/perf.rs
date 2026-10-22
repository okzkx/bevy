//! 性能读数与变更形态方法族（5.5）：`perf_stats`（分位数 + 分段均值 + 显存/
//! 规模读数）、`perf_samples`（原始样本导出，离线汇总用）、`workload`（变更
//! 形态开关写方法）——压测矩阵的采集与注入通道。
//!
//! 红线边界：三个方法只读账面/只写工作负载开关，**读数永远不回灌绘制决策**
//!（诊断性回读边界照 5.2 拍板）。perf 读数 = PerfLedger 环形窗口快照（Last
//! 帧循环逐帧 push，BRP handler 在 RemoteLast 读——窗口内含至本帧上帧的样本）。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    scene::{InstanceLedger, mechanism::WorkloadSettings},
    render::{GpuTimer, ImageCache, MeshPool, PerfLedger},
};

use super::{invalid_params, BrpResult};

/// 分位数三元组（p50/p95/p99）+ 均值，从 PerfLedger 现算。
fn percentile_block(ledger: &PerfLedger, field: impl Fn(&crate::render::FrameSample) -> f32) -> Value {
    let values: Vec<f32> = ledger.snapshot().iter().map(&field).collect();
    let pick = |p: f32| ledger.percentile(p, &field).map(|v| (v * 1000.0).round() / 1000.0);
    let mean = if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f32>() / values.len() as f32
    };
    json!({
        "p50": pick(0.50),
        "p95": pick(0.95),
        "p99": pick(0.99),
        "mean": (mean * 1000.0).round() / 1000.0,
        "n": values.len(),
    })
}

/// `ash_renderer/perf_stats`：帧时间分位数 + CPU 分段均值 + 显存/规模读数
///（只读）。分位数按 PerfLedger 当前环形窗口现算——窗口 = 最近 2048 帧。
///
/// # Errors
/// 诊断资源缺席（宿主未装配，组件错误级）。
#[system]
pub(crate) fn perf_stats(
    In(_params): In<Option<Value>>,
    perf: Res<PerfLedger>,
    timer: Res<GpuTimer>,
    ledger: Res<InstanceLedger>,
    pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
    workload: Res<WorkloadSettings>,
) -> BrpResult {
    let ((v_used, v_cap), (i_used, i_cap)) = pool.usage();
    let last = perf.snapshot().pop();
    Ok(json!({
        "cpu_total_ms": percentile_block(&perf, |s| s.cpu_total_ms),
        "frame_wall_ms": percentile_block(&perf, |s| s.frame_wall_ms),
        "gpu_total_ms": percentile_block(&perf, |s| s.gpu_total_ms),
        "gpu_lighting_ms": percentile_block(&perf, |s| s.gpu_lighting_ms),
        "gpu_compute_ms": percentile_block(&perf, |s| s.gpu_compute_ms),
        "gpu_hiz_ms": percentile_block(&perf, |s| s.gpu_hiz_ms),
        "sections_mean": {
            "wait_slot_ms": mean_of(&perf, |s| s.wait_slot_ms),
            "acquire_ms": mean_of(&perf, |s| s.acquire_ms),
            "assemble_ms": mean_of(&perf, |s| s.assemble_ms),
            "rings_ms": mean_of(&perf, |s| s.rings_ms),
            "record_submit_ms": mean_of(&perf, |s| s.record_submit_ms),
            "present_ms": mean_of(&perf, |s| s.present_ms),
        },
        "latest": last.map(|s| json!({
            "frame_wall_ms": s.frame_wall_ms,
            "cpu_total_ms": s.cpu_total_ms,
            "gpu_total_ms": s.gpu_total_ms,
            "instance_rows": s.instance_rows,
            "visible_rows": s.visible_rows,
        })),
        "scale": {
            "instance_rows": last.as_ref().map(|s| s.instance_rows),
            "visible_rows": last.as_ref().map(|s| s.visible_rows),
            "ledger_rows": ledger.len(),
            "object_slots": ledger.stats.object_slots,
            "material_slots": ledger.stats.material_slots,
            "scan_candidates": ledger.stats.scan_candidates,
        },
        "memory": {
            "mesh_vertex_used": v_used,
            "mesh_vertex_capacity": v_cap,
            "mesh_index_used": i_used,
            "mesh_index_capacity": i_cap,
            "image_resident_count": image_cache.resident_count(),
            "image_resident_bytes": image_cache.resident_bytes(),
            "image_graveyard": image_cache.graveyard_len(),
            "upload_bytes_total": ledger.stats.upload_bytes_total,
        },
        "workload": { "enabled": workload.enabled, "rows": workload.rows },
        "gpu_timer_enabled": timer.enabled,
        "staleness_note": "分位数 = PerfLedger 环形窗口现算（Last 帧循环逐帧 push）；GPU 区间 = 本槽两帧前的提交",
    }))
}

/// `ash_renderer/perf_samples`：原始样本导出（`{"last": N}` 取最近 N 条，
/// 缺省 256，上限 2048 = 窗口容量）——报告期离线汇总（CSV/表格）的数据源。
///
/// # Errors
/// 参数不合法（last 非法值）。
#[system]
pub(crate) fn perf_samples(In(params): In<Option<Value>>, perf: Res<PerfLedger>) -> BrpResult {
    let last = match params {
        Some(v) => {
            let p: LastParams = serde_json::from_value(v)
                .map_err(|e| invalid_params(format!("params 不合法：{e}")))?;
            p.last.unwrap_or(256)
        }
        None => 256,
    };
    if last > 2048 {
        return Err(invalid_params("last 上限 2048（窗口容量）"));
    }
    let all = perf.snapshot();
    let start = all.len().saturating_sub(last);
    let rows: Vec<Value> = all[start..]
        .iter()
        .map(|s| {
            json!({
                "frame_wall_ms": s.frame_wall_ms,
                "cpu_total_ms": s.cpu_total_ms,
                "wait_slot_ms": s.wait_slot_ms,
                "acquire_ms": s.acquire_ms,
                "assemble_ms": s.assemble_ms,
                "rings_ms": s.rings_ms,
                "record_submit_ms": s.record_submit_ms,
                "present_ms": s.present_ms,
                "gpu_compute_ms": s.gpu_compute_ms,
                "gpu_lighting_ms": s.gpu_lighting_ms,
                "gpu_hiz_ms": s.gpu_hiz_ms,
                "gpu_total_ms": s.gpu_total_ms,
                "instance_rows": s.instance_rows,
                "visible_rows": s.visible_rows,
            })
        })
        .collect();
    Ok(json!({ "total": all.len(), "returned": rows.len(), "samples": rows }))
}

/// `ash_renderer/workload`：变更形态开关写（压测矩阵的少量变化/全动态注入）。
/// `rows = 0` 或 `enabled = false` = 零变化。返回写后读回（对账用）。
///
/// # Errors
/// 参数缺失/不合法。
#[system]
pub(crate) fn workload(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 enabled 与可选 rows"))?;
    #[derive(serde::Deserialize)]
    struct WorkloadParams {
        enabled: bool,
        #[serde(default)]
        rows: Option<usize>,
    }
    let p: WorkloadParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let mut settings = world.resource_mut::<WorkloadSettings>();
    settings.enabled = p.enabled;
    if let Some(rows) = p.rows {
        settings.rows = rows;
    }
    let readback = *settings;
    Ok(json!({ "workload": { "enabled": readback.enabled, "rows": readback.rows } }))
}

/// 字段均值（两位小数；空窗口返回 0）。
fn mean_of(ledger: &PerfLedger, field: impl Fn(&crate::render::FrameSample) -> f32) -> f64 {
    let values: Vec<f32> = ledger.snapshot().iter().map(field).collect();
    if values.is_empty() {
        return 0.0;
    }
    ((values.iter().sum::<f32>() / values.len() as f32) * 1000.0).round() as f64 / 1000.0
}

#[derive(serde::Deserialize)]
struct LastParams {
    #[serde(default)]
    last: Option<usize>,
}
