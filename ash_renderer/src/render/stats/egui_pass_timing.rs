//! egui pass 计时读数：overlay 的 pass 写、BRP `monitor_stats` 读（资源由 overlay 插入，
//! 没装 overlay 时不存在）。

use bevy::prelude::*;

/// 最近一次 egui pass 的墙钟耗时。它在 Update 里跑，不在 draw_frame 的计时范围内，单独记录
/// 用来排查 FPS 下降。
#[derive(Resource, Default)]
pub struct EguiPassTiming {
    /// 最近一次 egui pass 的墙钟毫秒（两层 pass 合计；跳过帧为 0）。
    pub last_ms: f32,
}
