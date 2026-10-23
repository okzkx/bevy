//! overlay 业务半边——工具窗口：全部调试 UI 合并成一个窗口，左侧分组导航、
//! 右侧当前页内容。窗口默认折叠成标题栏，点标题展开；坐标轴指示器不进本窗口，
//! 永远显示（[`crate::overlay::widgets::axis_gizmo`]）。
//!
//! 加新页 = 本目录独立 rs 文件写页面 struct（`show(self, ui)` 只画内容，不建窗口）
//! + [`ToolPage`] 加一项并挂进 [`GROUPS`] + framework pass 的页面分发加一个分支。

pub(super) mod culling;
pub(super) mod entity_tree;
pub(super) mod frame_stats;
pub(super) mod monitor;
pub(super) mod pipeline;
pub(super) mod shading;

use bevy::prelude::*;

/// 工具窗口的页面。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(in crate::overlay) enum ToolPage {
    Shading,
    Culling,
    Pipeline,
    FrameStats,
    Monitor,
    EntityTree,
}

impl ToolPage {
    fn title(self) -> &'static str {
        match self {
            Self::Shading => "着色模式",
            Self::Culling => "剔除与 LOD",
            Self::Pipeline => "管线控制",
            Self::FrameStats => "帧统计",
            Self::Monitor => "持续监控",
            Self::EntityTree => "实体层级树",
        }
    }
}

/// 导航分组：(组名, 组内页面)，按显示顺序。
const GROUPS: &[(&str, &[ToolPage])] = &[
    ("渲染", &[ToolPage::Shading, ToolPage::Culling, ToolPage::Pipeline]),
    ("性能", &[ToolPage::FrameStats, ToolPage::Monitor]),
    ("场景", &[ToolPage::EntityTree]),
];

/// 工具窗口的导航状态：当前页 + 各分组折叠态（下标对应 [`GROUPS`]）。
#[derive(Resource)]
pub(in crate::overlay) struct ToolWindowState {
    page: ToolPage,
    group_open: [bool; GROUPS.len()],
}

impl Default for ToolWindowState {
    fn default() -> Self {
        Self { page: ToolPage::FrameStats, group_open: [true; GROUPS.len()] }
    }
}

/// 工具窗口外壳的一次构建输入：框架 pass 每帧组好，页面内容由 `show` 的闭包按页分发。
pub(in crate::overlay) struct ToolWindow<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
    pub(in crate::overlay) state: &'a mut ToolWindowState,
}

/// 左侧导航栏宽度（points）。
const NAV_WIDTH: f32 = 150.0;

impl ToolWindow<'_> {
    pub(in crate::overlay) fn show(self, page_ui: impl FnOnce(&mut egui::Ui, ToolPage)) {
        let Self { ctx, state } = self;
        egui::Window::new("工具窗口")
            .default_open(false)
            .default_pos(egui::pos2(12.0, 12.0))
            .default_size(egui::vec2(720.0, 480.0))
            .min_size(egui::vec2(420.0, 240.0))
            .resizable(true)
            .show(ctx, |ui| {
                let height = ui.available_height();
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(NAV_WIDTH, height),
                        egui::Layout::top_down_justified(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("工具窗口导航")
                                .auto_shrink([false, false])
                                .show(ui, |ui| show_nav(ui, state));
                        },
                    );
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.heading(state.page.title());
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt(("工具窗口页面", state.page))
                            .auto_shrink([false, false])
                            .show(ui, |ui| page_ui(ui, state.page));
                    });
                });
            });
    }
}

/// 导航树：分组标题点击折叠/展开，页面项点击切页，当前页高亮。
fn show_nav(ui: &mut egui::Ui, state: &mut ToolWindowState) {
    for (i, (group, pages)) in GROUPS.iter().enumerate() {
        let open = &mut state.group_open[i];
        let arrow = if *open { "▼" } else { "▶" };
        if ui
            .selectable_label(false, egui::RichText::new(format!("{arrow} {group}")).strong())
            .clicked()
        {
            *open = !*open;
        }
        if !*open {
            continue;
        }
        ui.indent(group, |ui| {
            for &page in *pages {
                if ui.selectable_label(state.page == page, page.title()).clicked() {
                    state.page = page;
                }
            }
        });
    }
}
