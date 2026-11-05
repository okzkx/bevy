//! 一帧怎么跑：帧槽与同步（`frame_pool`）、功能输入集合（`render_features`）、场景资源
//! （`scene_resources`）、整帧录制编排（`record`），以及影响整帧的设置（帧率同步、着色调试
//! 模式）。在 `passes` 之上，把各 pass 串成一帧。

pub(super) mod record;
pub(super) mod frame_pool;
pub(super) mod render_features;
pub(super) mod scene_resources;
pub(super) mod frame_sync_settings;
pub(super) mod render_mode;
