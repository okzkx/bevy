//! indirect 闭环读数方法族（5.3）：`draw_stats` 暴露录制层绘制调用数、GPU 命令
//! 条数、instanceCount=0 条数与账本走访口径——判定线 1（绘制调用不随 N 增长）
//! 与判定线 2（不可见零 GPU 工作）的证据通道。
//!
//! 红线边界（判定线 3）：本方法只读诊断账面，**不提供任何"按读数改场景"的
//! 通道**——读数永远不回灌绘制决策（诊断性回读边界照 5.2 拍板，此处单独标注）。
//! 读数是 draw_frame 上一帧的快照（Last 系统内刷新，handler 在 RemoteLast 读取
//! ——陈旧一帧，与 cull_stats 的两帧陈旧同族但更浅）。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::vulkan::{CullDiagnostics, IndirectStats};

use super::BrpResult;

/// `ash_renderer/draw_stats`：indirect 闭环读数（只读）。返回绘制调用数、命令
/// 条数、不可见命令条数与 CPU 走访/暂缓口径，附剔除读数快照。
///
/// # Errors
/// 诊断资源缺席（宿主未装配，组件错误级）。
#[system]
pub(crate) fn draw_stats(In(_params): In<Option<Value>>, cull_diag: Res<CullDiagnostics>, indirect: Res<IndirectStats>) -> BrpResult {
    Ok(json!({
        "draw_calls_recorded": indirect.draw_calls_recorded,
        "command_count": indirect.command_count,
        "zero_instance_count": indirect.zero_instance_count,
        "zero_instance_note": "诊断性读数，不驱动绘制（5.2 拍板口径）",
        "walk_rows": indirect.walk_rows,
        "pending_rows": indirect.pending_rows,
        "cull": {
            "enabled": cull_diag.enabled,
            "bound": cull_diag.bound,
            "visible_count": cull_diag.visible_count,
            "marks_frame": cull_diag.marks_frame,
        },
        "staleness_note": "读数 = draw_frame 上一帧快照（RemoteLast 读取，陈旧一帧）",
    }))
}
