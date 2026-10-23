//! 窗口内调试 UI：egui 裸接 ash，自含插件（照 scene 样式）。
//!
//! **边界**：本模块 = 画在 3D 画面之上的**屏幕空间 egui 2D 层**，全部形状走
//! egui→epaint→ash 一条绘制路径；3D 场景内的 gizmo（拾取/描边/物体中心拖拽
//! 手柄，外置拓展条目 4）有独立渲染路径，**不归这里**。
//!
//! 禁渲染宿主上 `bevy_egui` 挂不上（依赖 bevy_render 的 render graph），egui 官方
//! 后端只有 wgpu/glow——本模块自己接：[`framework`] 持有 [`egui::Context`] 并在
//! Update 跑一遍 egui pass（纯 CPU，零 Vulkan 符号），产 [`EguiFrame`] 交给
//! [`framework::paint`]（draw_frame 内调用的绘制半边：图集镜像整传 + 顶点环写入
//! → `UiPaint`，Vulkan 调用全走 [`crate::render`] 出口）。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`framework`] | 框架半边："怎么跑"——egui 状态（Context/字体含 CJK）+ Update 的 egui pass + 输入桥 + 绘制半边，产 [`EguiFrame`] |
//! | [`tool_window`] | 业务半边："画什么"——工具窗口（左侧分组导航 + 右侧页面：着色/剔除/管线/帧统计/监控/实体树） |
//! | [`widgets`] | 业务半边：屏幕空间非窗口 widget（右上角坐标轴指示器，永远显示；加载弹窗） |
//!
//! 与下层的接口：GPU 侧 UI 账面（`AtlasGpu`/`UiVertexRing`）、着色模式、监控账
//! 与 pass 自计时等数据契约住 [`crate::render`]；Transform 编辑与层级树索引的
//! 机制层、输入仲裁窄接口 `PointerCapture` 住 [`crate::scene::mechanism`]——
//! 本模块只消费/写入这些契约，不被下层与 remote 引用。
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.7-egui调试GUI/`、
//! `.agents/docs/3-静态取数链路/3.10-egui实体层级树/`、
//! `.agents/docs/3-静态取数链路/3.11-egui实体编辑/`。

mod framework;
mod tool_window;
mod widgets;

pub use framework::paint::AtlasMirror;
pub use framework::ui::{EguiFrame, EguiState, OverlayPlugin};

pub(crate) use framework::paint::{paint_overlay, OverlayLogState, UiDrawData};
