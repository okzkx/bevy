//! overlay 屏幕空间 widget：非窗口的常驻叠加件（如坐标轴指示器）。与
//! [`crate::overlay::tool_window`] 同属业务半边，纯 egui 形状、零独立渲染路径。

pub(super) mod axis_gizmo;
pub(super) mod load_popup;
