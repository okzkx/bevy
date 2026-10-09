//! 调试窗口总控（业务面板）：各调试 UI 窗口的显隐开关住 [`DebugWindowsOpen`]，
//! 本窗口逐项 checkbox——"总有一个能再开别的"的总闸，自身不可关。
//!
//! 加新调试窗口 = 独立 rs 文件 struct（照 [`super::entity_tree_window`] 样式）
//! + 本资源加一个开关字段 + 这里加一行 checkbox + [`super::ui`] pass 里加显示门。

use bevy::prelude::*;

/// 各调试 UI 窗口的显隐开关（总控面板写，`run_egui_pass` 的显示门读；窗口 [×]
/// 经 egui `Window::open` 借用同字段写回）。bool = 显示中。
#[derive(Resource)]
pub(super) struct DebugWindowsOpen {
    /// 帧统计/着色模式/渲染器统计窗口（3.7，[`super::debug_window`]）
    pub(super) stats: bool,
    /// 实体层级树窗口（3.10，[`super::entity_tree_window`]）
    pub(super) entity_tree: bool,
}

impl Default for DebugWindowsOpen {
    fn default() -> Self {
        // 默认全开：3.7 既有行为不变，3.10 新窗直接可见可验；关了从总控再开
        Self { stats: true, entity_tree: true }
    }
}

/// 总控窗口的一次构建输入：框架 pass 每帧组好。
pub(super) struct DebugHubWindow<'a> {
    pub(super) ctx: &'a egui::Context,
    /// 各窗口开关（checkbox 就地写）
    pub(super) open: &'a mut DebugWindowsOpen,
}

impl DebugHubWindow<'_> {
    /// 总控面板本体：每个调试 UI 一行开关。
    pub(super) fn show(self) {
        let Self { ctx, open } = self;
        egui::Window::new("调试窗口总控")
            .default_pos(egui::pos2(12.0, 210.0))
            .default_width(170.0)
            .show(ctx, |ui| {
                ui.checkbox(&mut open.stats, "帧统计（ash 调试）");
                ui.checkbox(&mut open.entity_tree, "实体层级树");
            });
    }
}
