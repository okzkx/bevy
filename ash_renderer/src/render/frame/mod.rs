//! 一帧怎么跑：帧槽与同步（`frame_pool`）、整帧录制编排（`frame_draw`），以及
//! 影响整帧的设置（帧率同步、着色调试模式）。在 `passes` 之上，把各 pass 串成一帧。

pub(super) mod frame_draw;
pub(super) mod frame_pool;
pub(super) mod frame_sync_settings;
pub(super) mod render_mode;
