//! indirect 闭环读数方法族：`draw_stats` 暴露录制层绘制调用数、GPU 命令条数、
//! instanceCount=0 条数与账本走访口径——"绘制调用不随 N 增长"与"不可见零 GPU
//! 工作"的证据通道；`drawlist_verify_set` 开关组装对照模式。
//!
//! 边界：只读诊断账面，读数不回灌绘制决策。读数是 draw_frame 上一帧的快照（Last
//! 内刷新，handler 在 RemoteLast 读取，陈旧一帧）。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::{DrawListVerify, InstanceLedger};
use crate::render::{CullDiagnostics, IndirectStats};

use super::params::parse_enabled;

/// 组装对照模式的计量读回（draw_stats 与 drawlist_verify_set 共用）。
fn verify_json(verify: &DrawListVerify) -> Value {
    json!({
        "enabled": verify.enabled,
        "frames_compared_total": verify.frames_compared_total,
        "mismatches_total": verify.mismatches_total,
        "last_mismatch": verify.last_mismatch,
    })
}

/// `ash_renderer/draw_stats`：indirect 闭环读数（只读）。返回绘制调用数、命令
/// 条数、不可见命令条数与 CPU 走访/暂缓口径，附剔除读数快照与组装增量账的
/// 计量（重建/重估/对照模式）。
///
/// # Errors
/// 诊断资源缺席（系统参数校验失败，bevy_remote 回 internal）。
#[system]
pub(crate) fn draw_stats(
    In(_params): In<Option<Value>>,
    cull_diag: Res<CullDiagnostics>,
    indirect: Res<IndirectStats>,
    ledger: Res<InstanceLedger>,
    verify: Res<DrawListVerify>,
) -> BrpResult {
    let assembly = ledger.assembly();
    Ok(json!({
        "draw_calls_recorded": indirect.draw_calls_recorded,
        "command_count": indirect.command_count,
        "zero_instance_count": indirect.zero_instance_count,
        "zero_instance_note": "诊断性读数，不驱动绘制",
        "walk_rows": indirect.walk_rows,
        "pending_rows": indirect.pending_rows,
        "assembly": {
            "rebuilds_total": assembly.rebuilds_total,
            "reevals_total": assembly.reevals_total,
            "drained_rows_total": assembly.drained_rows_total,
            "last_drain_rows": assembly.last_drain_rows,
        },
        "verify": verify_json(&verify),
        "cull": {
            "enabled": cull_diag.enabled,
            "bound": cull_diag.bound,
            "visible_count": cull_diag.visible_count,
            "marks_frame": cull_diag.marks_frame,
        },
        "staleness_note": "读数 = draw_frame 上一帧快照（RemoteLast 读取，陈旧一帧）",
    }))
}

/// `ash_renderer/drawlist_verify_set`：组装对照模式开关写。
/// 开启后帧循环逐帧跑全量参照走访与增量缓存比对——只进计量，不驱动绘制。
/// 返回写后读回（对账用）。
///
/// # Errors
/// 参数缺失/不合法。
#[system]
pub(crate) fn drawlist_verify_set(
    In(params): In<Option<Value>>,
    mut verify: ResMut<DrawListVerify>,
) -> BrpResult {
    verify.enabled = parse_enabled(params)?;
    Ok(json!({ "verify": verify_json(&verify) }))
}
