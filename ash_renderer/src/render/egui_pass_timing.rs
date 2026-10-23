//! egui pass 自计时读数契约：overlay 的 pass 写、BRP `monitor_stats` 读
//!（资源由 overlay 插入，overlay 未装时缺席）。

use bevy::prelude::*;

/// egui pass 自计时：最近一次 pass 的墙钟耗时——Update 侧在
/// draw_frame 计量之外，FPS 恶化取证的 Update 半边仪器。
#[derive(Resource, Default)]
pub struct EguiPassTiming {
    /// 最近一次 egui pass 的墙钟毫秒（两层 pass 合计；跳过帧为 0）。
    pub last_ms: f32,
}
