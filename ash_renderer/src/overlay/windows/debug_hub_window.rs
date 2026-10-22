//! 调试窗口总控（业务面板）：各调试 UI 窗口的显隐开关住 [`DebugWindowsOpen`]，
//! 本窗口逐项 checkbox——"总有一个能再开别的"的总闸，自身不可关。
//!
//! 加新调试窗口 = 独立 rs 文件 struct（照 [`super::entity_tree_window`] 样式），
//! 加本资源一个开关字段 + 这里一行 checkbox +
//! [`crate::overlay::framework::ui`] pass 一个显示门。

use bevy::prelude::*;

/// 各调试 UI 窗口的显隐开关（总控面板写，`run_egui_pass` 的显示门读；窗口 [×]
/// 经 egui `Window::open` 借用同字段写回）。bool = 显示中。
///
/// 默认全关（derive bool = false）：屏幕上只留总控——其余窗口需要时从总控
/// 勾开，首次可见可验的职责交给总控开关，不靠默认摊开。
#[derive(Resource, Default)]
pub(in crate::overlay) struct DebugWindowsOpen {
    /// 帧统计/着色模式/渲染器统计窗口（[`super::debug_window`]）
    pub(in crate::overlay) stats: bool,
    /// 持续监控窗口（[`super::monitor_window`]）
    pub(in crate::overlay) monitor: bool,
    /// 实体层级树窗口（[`super::entity_tree_window`]）
    pub(in crate::overlay) entity_tree: bool,
    /// 右上角坐标轴指示器（[`crate::overlay::widgets::axis_gizmo`]，纯显示不可交互）
    pub(in crate::overlay) axis_gizmo: bool,
}

/// 总控窗口的一次构建输入：框架 pass 每帧组好。
pub(in crate::overlay) struct DebugHubWindow<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
    /// 各窗口开关（checkbox 就地写）
    pub(in crate::overlay) open: &'a mut DebugWindowsOpen,
}

impl DebugHubWindow<'_> {
    /// 总控面板本体：每个调试 UI 一行开关。
    pub(in crate::overlay) fn show(self) {
        let Self { ctx, open } = self;
        egui::Window::new("调试窗口总控")
            .default_pos(egui::pos2(12.0, 210.0))
            .default_width(170.0)
            .show(ctx, |ui| {
                ui.checkbox(&mut open.stats, "帧统计（ash 调试）");
                ui.checkbox(&mut open.monitor, "持续监控");
                ui.checkbox(&mut open.entity_tree, "实体层级树");
                ui.checkbox(&mut open.axis_gizmo, "坐标轴指示器");
            });
    }
}
