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
//! | [`ui`] | egui 状态（Context/字体含 CJK）+ Update 的 egui pass + 调试窗口 + [`RenderMode`] 资源 |
//! | [`input`] | 输入桥：bevy 输入事件 → egui `RawInput`（含 KeyCode→egui Key 映射） |
//! | [`paint`] | 绘制半边：图集 CPU 镜像/整传新槽/graveyard + 顶点环 + `paint_overlay` 产 `UiPaint` |
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.7-egui调试GUI/`。

mod input;
mod paint;
mod ui;

pub use paint::{AtlasGpu, AtlasMirror, UiVertexRing};
pub use ui::{EguiFrame, EguiState, OverlayPlugin, RenderMode};

pub(crate) use paint::{paint_overlay, OverlayLogState, UiDrawData};
