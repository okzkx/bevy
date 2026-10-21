//! 持续监控读数方法族（7.5）：`monitor_stats`——fps/进程内存/系统内存/显存
//! 堆级预算 + 渲染器内部账，一次读全。与 egui 监控窗口同源（读
//! [`MonitorHistory`] 同一本账），供 tools/monitor.py 无人值守轮询采样，
//! 取证与修复前后对照的数据通道。
//!
//! 红线边界照 perf 族：只读账面，读数不回灌绘制决策。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    overlay::MonitorHistory,
    vulkan::{Context, ImageCache, MeshPool},
};

use super::BrpResult;

/// `ash_renderer/monitor_stats`：持续监控快照（只读）。fps 取监控历史最近
/// 采样（≤0.5s 陈旧）；显存堆级读数现查（VK_EXT_memory_budget 缺席为 null）；
/// 内部账 = MeshPool/ImageCache 进程自身口径。
///
/// # Errors
/// 诊断资源缺席（宿主未装配，组件错误级）。
#[system]
pub(crate) fn monitor_stats(
    In(_params): In<Option<Value>>,
    history: Res<MonitorHistory>,
    ctx: Option<Res<Context>>,
    mesh_pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
) -> BrpResult {
    let last = history.samples.last().copied();
    let vram = ctx.as_deref().and_then(Context::memory_budget).map(|heaps| {
        // 设备局部堆 usage/budget 求和（监控窗口同款口径）
        heaps
            .iter()
            .filter(|h| h.device_local)
            .fold((0u64, 0u64), |(u, b), h| (u + h.usage, b + h.budget))
    });
    let ((v_used, v_cap), (i_used, i_cap)) = mesh_pool.usage();
    Ok(json!({
        "fps": last.map(|s| s.fps),
        "ms": last.map(|s| s.ms),
        "elapsed_secs": last.map(|s| s.elapsed),
        "history_len": history.samples.len(),
        "proc_working_set": last.and_then(|s| s.proc_working_set),
        "sys_total": last.and_then(|s| s.sys).map(|(t, _)| t),
        "sys_avail": last.and_then(|s| s.sys).map(|(_, a)| a),
        "vram_device_local": vram.map(|(u, b)| json!({"usage": u, "budget": b})),
        "internal": {
            "mesh_vertex_used": v_used,
            "mesh_vertex_capacity": v_cap,
            "mesh_index_used": i_used,
            "mesh_index_capacity": i_cap,
            "image_resident_count": image_cache.resident_count(),
            "image_resident_bytes": image_cache.resident_bytes(),
            "image_graveyard": image_cache.graveyard_len(),
        },
    }))
}
