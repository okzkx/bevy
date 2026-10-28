//! 帧率同步设置契约：开关 + 主屏刷新率读数。`AshHostPlugin` 插入，
//! driver 的 `apply_frame_sync` 每帧回填刷新率并按开关改写 winit update
//! mode；overlay 监控页与 BRP `frame_sync`/`monitor_stats` 读写本资源。

use bevy::prelude::*;

/// 帧率同步设置：开关 + 主屏刷新率读数。
///
/// `enabled` 默认 true = 首次运行自动同步（显示器信息就绪后自动对齐）；
/// `refresh_millihertz` 由 driver 的 `apply_frame_sync` 每帧回填（UI/BRP 只读），
/// None = 刷新率不可用（保持现状 1/60，读数如实显示"不可用"）。
#[derive(Resource, Debug)]
pub struct FrameSyncSettings {
    pub enabled: bool,
    pub refresh_millihertz: Option<u32>,
}

impl Default for FrameSyncSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_millihertz: None,
        }
    }
}
