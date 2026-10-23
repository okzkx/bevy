//! overlay 业务半边——调试窗口：独立 rs 文件 struct（照 [`super::entity_tree_window`]
//! 样式）+ [`DebugWindowsOpen`] 加开关字段 + 总控 checkbox + framework pass 显示门。
//! 屏幕空间非窗口类 widget 在 [`crate::overlay::widgets`]。

pub(super) mod debug_hub_window;
pub(super) mod debug_window;
pub(super) mod entity_tree_window;
pub(super) mod monitor_window;
pub(super) mod pipeline_window;
