//! 窗口内调试 UI（3.7）：egui 裸接 ash，自含插件（照 scene 样式）。
//!
//! 禁渲染宿主上 `bevy_egui` 挂不上（依赖 bevy_render 的 render graph），egui 官方
//! 后端只有 wgpu/glow——本模块自己接：[`ui`] 持有 [`egui::Context`] 并在 Update
//! 跑一遍 egui pass（纯 CPU，零 Vulkan 符号），产 [`EguiFrame`] 交给 [`paint`]
//!（draw_frame 内调用的绘制半边：图集镜像整传 + 顶点环写入 → `UiPaint`，Vulkan
//! 调用全走 [`crate::vulkan`] 出口）。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`ui`] | 框架半边：egui 状态（Context/字体含 CJK）+ Update 的 egui pass，产 [`EguiFrame`] |
//! | [`debug_window`] | 业务半边：帧统计窗口（fps/着色模式单选/渲染器统计）+ [`RenderMode`] 资源 |
//! | [`debug_hub_window`] | 业务半边：调试窗口总控——各窗口显隐开关 [`DebugWindowsOpen`] |
//! | [`entity_tree_window`] | 业务半边：实体层级树 + 属性快照采集 + 属性区（3.10/3.11，[`SelectedEntity`] 点选） |
//! | [`transform_edit`] | Transform 编辑封装（3.11.2）：编辑队列 + 唯一实现，3.12 的 BRP 写方法复用 |
//! | [`input`] | 输入桥：bevy 输入事件 → egui `RawInput`（含 KeyCode→egui Key 映射） |
//! | [`paint`] | 绘制半边：图集 CPU 镜像/整传新槽/graveyard + 顶点环 + `paint_overlay` 产 `UiPaint` |
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.7-egui调试GUI/`、
//! `.agents/docs/3-静态取数链路/3.10-egui实体层级树/`、
//! `.agents/docs/3-静态取数链路/3.11-egui实体编辑/`。

mod debug_hub_window;
mod debug_window;
mod entity_tree_window;
mod input;
mod paint;
pub(crate) mod transform_edit;
mod ui;

pub use debug_window::RenderMode;
pub use paint::{AtlasGpu, AtlasMirror, UiVertexRing};
pub use ui::{EguiFrame, EguiState, OverlayPlugin};

pub(crate) use paint::{paint_overlay, OverlayLogState, UiDrawData};
// 3.12 的 BRP scene_tree 复用树窗口的行数据、相关性过滤与节点命名——
// CLI 树与 egui 树看到同一棵树、同一套"相关"语义，不养两份逻辑。
pub(crate) use entity_tree_window::{EntityRow, is_relevant, node_label};
