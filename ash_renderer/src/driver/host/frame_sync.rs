//! 帧率同步底座（7.6）：把宿主帧循环节奏从写死的 reactive(1/60) 变成
//! 可观察可控制——[`apply_frame_sync`] 每帧读主窗所在显示器的刷新率回填
//! [`FrameSyncSettings::refresh_millihertz`]，并按同步开关改写
//! `WinitSettings` 两档 update mode（bevy_winit 每轮循环重提取
//! update_mode，写后下轮循环生效）。
//!
//! 边界：只动 winit update 节流，不动 swapchain present mode（MAILBOX 保持）
//! ——"同步 vsync"= 帧循环节奏对齐显示器刷新率，不是 present mode 切换。

use std::time::Duration;

use bevy::prelude::*;
use bevy::window::{Monitor, OnMonitor, PrimaryMonitor, PrimaryWindow};
use bevy::winit::{UpdateMode, WinitSettings};

use ash_macros::system;

/// 帧率同步设置：开关 + 主屏刷新率读数。
///
/// `enabled` 默认 true = 首次运行自动同步（显示器信息就绪后自动对齐）；
/// `refresh_millihertz` 由 [`apply_frame_sync`] 每帧回填（UI/BRP 只读），
/// None = 刷新率不可用（保持现状 1/60，读数如实显示"不可用"）。
#[derive(Resource, Debug)]
pub(crate) struct FrameSyncSettings {
    pub(crate) enabled: bool,
    pub(crate) refresh_millihertz: Option<u32>,
}

impl Default for FrameSyncSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_millihertz: None,
        }
    }
}

/// 帧率同步应用系统（Update）：读刷新率 → 回填读数 → 按开关算两档目标
/// update mode → 与当前 `WinitSettings` 逐位不等才写（避免逐帧 insert 的
/// change detection 噪声）。
///
/// 目标两档：同步开（有刷新率读数）聚焦/非聚焦都取 1/刷新率 wait，非聚焦
/// 保持 reactive_low_power 的节能形态（对齐历史两档同 1/60 的对应关系）；
/// 同步关联焦 `Continuous` 跑满（MAILBOX 无背压）、非聚焦保
/// reactive_low_power(1/60) 节能。刷新率不可用（None）时同步开档位退回
/// 现状 1/60，与"保持现状"分支同值。
#[system]
pub(crate) fn apply_frame_sync(
    mut settings: ResMut<FrameSyncSettings>,
    mut winit: ResMut<WinitSettings>,
    window: Query<&OnMonitor, With<PrimaryWindow>>,
    monitors: Query<(Has<PrimaryMonitor>, &Monitor)>,
) {
    settings.refresh_millihertz = read_refresh_millihertz(&window, &monitors);
    let (focused, unfocused) = if !settings.enabled {
        (
            UpdateMode::Continuous,
            UpdateMode::reactive_low_power(Duration::from_secs_f64(1.0 / 60.0)),
        )
    } else {
        match settings.refresh_millihertz.filter(|mhz| *mhz > 0) {
            Some(mhz) => {
                let wait = Duration::from_secs_f64(1000.0 / f64::from(mhz));
                (
                    UpdateMode::reactive(wait),
                    UpdateMode::reactive_low_power(wait),
                )
            }
            None => (
                UpdateMode::reactive(Duration::from_secs_f64(1.0 / 60.0)),
                UpdateMode::reactive_low_power(Duration::from_secs_f64(1.0 / 60.0)),
            ),
        }
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
