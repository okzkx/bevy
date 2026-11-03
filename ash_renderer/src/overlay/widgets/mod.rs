//! overlay 屏幕空间 widget：非窗口的叠加件（坐标轴指示器、加载弹窗）。与
//! [`crate::overlay::tool_window`] 同属业务半边，纯 egui 形状、零独立渲染路径。

pub(super) mod axis_gizmo;
pub(super) mod load_popup;
