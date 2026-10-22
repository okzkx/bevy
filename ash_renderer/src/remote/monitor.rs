//! 持续监控读数方法族（7.5）：`monitor_stats`——fps/进程内存/系统内存/显存
//! 堆级预算 + 渲染器内部账，一次读全。与 egui 监控窗口同源（读
//! [`MonitorHistory`] 同一本账），供 tools/monitor.py 无人值守轮询采样，
//! 取证与修复前后对照的数据通道。7.6 增补：`frame_sync` 帧率同步开关
//! 读写 + monitor_stats 增补刷新率/开关两字段。
//!
//! 红线边界照 perf 族：只读账面，读数不回灌绘制决策。

use bevy::prelude::*;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    driver::host::FrameSyncSettings,
    overlay::{EguiPassTiming, MonitorHistory},
    render::{Context, ImageCache, MeshPool, PerfLedger},
};

use super::{BrpResult, invalid_params};

/// `ash_renderer/monitor_stats`：持续监控快照（只读）。fps 取监控历史最近
/// 采样（≤0.5s 陈旧）；显存堆级读数现查（VK_EXT_memory_budget 缺席为 null）；
/// 内部账 = MeshPool/ImageCache 进程自身口径。
///
/// # Errors
/// 诊断资源缺席（宿主未装配，组件错误级）。
#[expect(
    clippy::too_many_arguments,
    reason = "BRP 只读读数口的参数表即数据源清单:监控账/池/贴图缓存/帧账/egui计时/帧率同步逐项只读,拆参数束过度设计"
)]
#[system]
pub(crate) fn monitor_stats(
    In(_params): In<Option<Value>>,
    history: Res<MonitorHistory>,
    ctx: Option<Res<Context>>,
    mesh_pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
    perf: Res<PerfLedger>,
    egui_timing: Res<EguiPassTiming>,
    frame_sync: Res<FrameSyncSettings>,
) -> BrpResult {
    let last = history.samples.last().copied();
    let frame_wall = perf.snapshot().pop().map(|s| s.frame_wall_ms);
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
        "frame_wall_ms": frame_wall,
        "egui_pass_ms": egui_timing.last_ms,
        "ms": last.map(|s| s.ms),
        "elapsed_secs": last.map(|s| s.elapsed),
        "history_len": history.samples.len(),
        "refresh_millihertz": frame_sync.refresh_millihertz,
        "frame_sync_enabled": frame_sync.enabled,
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

/// `ash_renderer/frame_sync` 参数：`enabled` 可选——缺省只读。
#[derive(serde::Deserialize)]
struct FrameSyncParams {
    #[serde(default)]
    enabled: Option<bool>,
}

/// `ash_renderer/frame_sync`：帧率同步开关读写（7.6）。带 `enabled` 则写
/// 开关（写侧同 cull_set_enabled 家族形状），缺省只读；两种形态都返回
/// 当前 `{enabled, refresh_millihertz}` 供脚本 A/B 对账。
///
/// # Errors
/// 参数不合法。
#[system]
pub(crate) fn frame_sync_cmd(
    In(params): In<Option<Value>>,
    mut settings: ResMut<FrameSyncSettings>,
) -> BrpResult {
    if let Some(value) = params.filter(|v| !v.is_null()) {
        let p: FrameSyncParams =
            serde_json::from_value(value).map_err(|e| invalid_params(format!("params: {e}")))?;
        if let Some(enabled) = p.enabled {
            settings.enabled = enabled;
        }
    }
    Ok(json!({
        "enabled": settings.enabled,
        "refresh_millihertz": settings.refresh_millihertz,
    }))
}
