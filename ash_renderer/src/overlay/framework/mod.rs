//! overlay 框架半边："怎么跑"——egui 状态与 pass（[`ui`]）、输入桥（[`input`]）、
//! 绘制半边（[`paint`]：图集镜像/顶点环/`paint_overlay`）。窗口内容"画什么"
//! 在 [`crate::overlay::windows`] 与 [`crate::overlay::widgets`]。

pub(super) mod input;
pub(super) mod paint;
pub(super) mod ui;
