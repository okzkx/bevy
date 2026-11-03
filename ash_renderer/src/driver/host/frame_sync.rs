//! 帧率同步：[`apply_frame_sync`] 每帧读主窗所在显示器的刷新率回填
//! [`FrameSyncSettings::refresh_millihertz`]，并按同步开关改写 `WinitSettings` 两档
//! update mode（bevy_winit 每轮循环重新读取，写后下轮生效）。
//!
//! 边界：只动 winit update 节流，不动 swapchain present mode（保持 MAILBOX）——
//! "同步"指帧循环节奏对齐显示器刷新率，不是切 present mode。

use std::time::Duration;

use bevy::prelude::*;
use bevy::window::{Monitor, OnMonitor, PrimaryMonitor, PrimaryWindow};
use bevy::winit::{UpdateMode, WinitSettings};

use ash_macros::system;

use crate::render::FrameSyncSettings;

/// 刷新率读数缺席时的 update 间隔。
pub(super) fn fallback_wait() -> Duration {
    Duration::from_secs_f64(1.0 / 60.0)
}

/// 读刷新率 → 回填读数 → 算两档目标 update mode → 与当前 `WinitSettings` 不等才写
/// （避免逐帧写入触发 change detection）。
///
/// 同步开：聚焦 reactive、非聚焦 reactive_low_power，间隔都取 1/刷新率（无读数用
/// [`fallback_wait`]）。同步关：聚焦 `Continuous` 跑满（MAILBOX 无背压），非聚焦
/// reactive_low_power([`fallback_wait`]) 节能。
#[system]
pub(crate) fn apply_frame_sync(
    mut settings: ResMut<FrameSyncSettings>,
    mut winit: ResMut<WinitSettings>,
    window: Query<&OnMonitor, With<PrimaryWindow>>,
    monitors: Query<(Has<PrimaryMonitor>, &Monitor)>,
) {
    settings.refresh_millihertz = read_refresh_millihertz(&window, &monitors);
    let (focused, unfocused) = if settings.enabled {
        let wait = settings
            .refresh_millihertz
            .filter(|mhz| *mhz > 0)
            .map_or_else(fallback_wait, |mhz| Duration::from_secs_f64(1000.0 / f64::from(mhz)));
        (UpdateMode::reactive(wait), UpdateMode::reactive_low_power(wait))
    } else {
        (UpdateMode::Continuous, UpdateMode::reactive_low_power(fallback_wait()))
    };
    if winit.focused_mode != focused || winit.unfocused_mode != unfocused {
        winit.focused_mode = focused;
        winit.unfocused_mode = unfocused;
    }
}

/// 刷新率读数：主窗 `OnMonitor` 目标的 `Monitor` 优先，`PrimaryMonitor`
/// 实体兜底，都取不到 None。`OnMonitor` 在窗口创建后才插入——头几帧 None
/// 属正常时序，读数就绪后自动跟上。
fn read_refresh_millihertz(
    window: &Query<&OnMonitor, With<PrimaryWindow>>,
    monitors: &Query<(Has<PrimaryMonitor>, &Monitor)>,
) -> Option<u32> {
    if let Ok(on_monitor) = window.single()
        && let Ok((_, monitor)) = monitors.get(on_monitor.0)
    {
        return monitor.refresh_rate_millihertz;
    }
    monitors
        .iter()
        .filter(|(primary, _)| *primary)
        .filter_map(|(_, monitor)| monitor.refresh_rate_millihertz)
        .next()
}
