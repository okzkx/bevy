//! overlay 业务半边——工具窗口：全部调试 UI 合并成一个窗口，左侧分组导航、
//! 右侧当前页内容。窗口默认折叠成标题栏，点标题展开；坐标轴指示器不进本窗口，
//! 永远显示（[`crate::overlay::widgets::axis_gizmo`]）。
//!
//! 加新页 = 本目录独立 rs 文件写页面 struct（`show(self, ui)` 只画内容，不建窗口）、
//! [`ToolPage`] 加一项并挂进 [`GROUPS`]、[`ToolWindowParams::show`] 的页面分发加
//! 一个分支（页面读写的资源进 [`ToolWindowParams`] 束）。
//!
//! 与框架半边的接口：framework pass 每帧调 [`ToolWindowParams::tick_snapshot`]
//!（节拍底座）、以 [`ToolWindowParams::changed`] 计入面板触发集、在 begin/end
//! pass 之间调 [`ToolWindowParams::show`]；资源由 [`insert_tool_window_resources`]
//! 在 Startup 落地。

pub(super) mod culling;
pub(super) mod entity_tree;
pub(super) mod frame_stats;
pub(super) mod monitor;
pub(super) mod pipeline;
pub(super) mod shading;

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::render::{
    BloomSettings, Context, CullDiagnostics, CullSettings, FrameSyncSettings, ImageCache,
    IndirectStats, LodDiagnostics, LodSettings, MeshPool, MonitorHistory, OcclusionDiagnostics,
    OcclusionSettings, PerfLedger, RenderMode, ShadowSettings, SmaaSettings,
};
use crate::scene::mechanism::{TransformEditQueue, WorkloadSettings};
use culling::CullingPage;
use entity_tree::{
    EntityTreePage, EntityTreeState, SelectedDetails, SelectedEntity, TransformDragCache,
};
use frame_stats::{ChunkRows, FpsMeter, FrameStatsPage, PerfPanelCache, RendererStats};
use monitor::MonitorPage;
use pipeline::PipelinePage;
use shading::ShadingPage;

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

/// 工具窗口外壳的一次构建输入：[`ToolWindowParams::show`] 每帧组好，页面内容由
/// `show` 的闭包按页分发。
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

/// 字节 → MiB（读数行显示口径）。
pub(super) fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// 字节 → GiB（读数行显示口径）。
pub(super) fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Startup：工具窗口各页的状态资源落地（着色模式由 driver 的宿主插件插入）。
pub(in crate::overlay) fn insert_tool_window_resources(commands: &mut Commands) {
    // 工具窗口导航状态：当前页 + 分组折叠
    commands.insert_resource(ToolWindowState::default());
    // 层级树点选：属性面板的编辑目标来源
    commands.insert_resource(SelectedEntity::default());
    // 树拍平缓存：脏信号观测器 + 维护系统重建，默认脏 = 首帧立树
    commands.insert_resource(EntityTreeState::default());
    // 属性快照：采集系统每帧重写，Startup 先落空壳
    commands.insert_resource(SelectedDetails::default());
    // 旋转拖拽缓存：拖拽期内冻结欧拉表示分支（防跨分支混写闪烁）
    commands.insert_resource(TransformDragCache::default());
    // 帧率显示平滑：0.5s 出一次平均快照
    commands.insert_resource(FpsMeter::default());
    // 持续监控历史：随 fps 快照节奏采样，窗口显隐不影响记账
    commands.insert_resource(MonitorHistory::default());
    // 分位数面板缓存：0.5s 重算，调试窗只读
    commands.insert_resource(PerfPanelCache::default());
}

/// 工具窗口各页的一次性参数束：统计、着色模式、导航状态、点选与层级树、各页
/// 开关与诊断读数合成一个 SystemParam——系统参数上限 16（FrameInput 同款式的
/// 束打法），新增页面件全走这里，不撑大 pass 签名。
#[derive(SystemParam)]
pub(in crate::overlay) struct ToolWindowParams<'w, 's> {
    stats: RendererStats<'w>,
    mode: ResMut<'w, RenderMode>,
    tool_state: ResMut<'w, ToolWindowState>,
    selected: ResMut<'w, SelectedEntity>,
    fps: ResMut<'w, FpsMeter>,
    /// 分位数面板缓存：0.5s 重算
    pct_cache: ResMut<'w, PerfPanelCache>,
    /// 持续监控历史：fps 快照刷新时采样入账
    history: ResMut<'w, MonitorHistory>,
    /// Vulkan 上下文（显存堆级读数；init 前空帧缺席）
    vulkan: Option<Res<'w, Context>>,
    /// 树拍平缓存：维护系统产，pass 只读；展开/收起写回
    tree_state: ResMut<'w, EntityTreeState>,
    /// 属性快照（采集系统产，只读消费）
    details: Res<'w, SelectedDetails>,
    /// 旋转拖拽缓存（拖拽期内冻结欧拉表示分支）
    drag_cache: ResMut<'w, TransformDragCache>,
    /// Transform 编辑排队（pass 产，清账系统消费）
    edit_queue: ResMut<'w, TransformEditQueue>,
    /// 剔除开关（checkbox 写，draw_frame 读）
    cull_settings: ResMut<'w, CullSettings>,
    /// 剔除诊断读数（draw_frame 每帧刷新，只读展示）
    cull_diag: Res<'w, CullDiagnostics>,
    /// 遮挡开关（checkbox 写，draw_frame 读）
    occlusion_settings: ResMut<'w, OcclusionSettings>,
    /// 遮挡诊断读数（draw_frame 每帧刷新，只读展示）
    occlusion_diag: Res<'w, OcclusionDiagnostics>,
    /// LOD 开关（checkbox 写，draw_frame 读）
    lod_settings: ResMut<'w, LodSettings>,
    /// LOD 诊断读数（draw_frame 每帧刷新，只读展示）
    lod_diag: Res<'w, LodDiagnostics>,
    /// indirect 闭环读数（draw_frame 每帧刷新，只读展示）
    indirect_stats: Res<'w, IndirectStats>,
    /// chunk 盘点（块数与成员总数计量行）
    chunks: ChunkRows<'w, 's>,
    /// 帧样本账（分位数/显存/注入读数）
    perf: Res<'w, PerfLedger>,
    /// 帧率同步开关与刷新率读数（checkbox 写，apply_frame_sync 读）
    frame_sync: ResMut<'w, FrameSyncSettings>,
    image_cache: Res<'w, ImageCache>,
    mesh_pool: Res<'w, MeshPool>,
    workload: Res<'w, WorkloadSettings>,
    /// 阴影开关（checkbox 写，draw_frame 读；BRP shadow_set 同源）
    shadow_settings: ResMut<'w, ShadowSettings>,
    /// Bloom/tonemap 开关与参数（checkbox/滑杆写；BRP bloom_set 同源）
    bloom_settings: ResMut<'w, BloomSettings>,
    /// SMAA 开关（checkbox 写；BRP smaa_set 同源）
    smaa_settings: ResMut<'w, SmaaSettings>,
}

impl ToolWindowParams<'_, '_> {
    /// 节拍底座（每帧调用，不隶属 pass）：fps 快照 + 监控采样 + 分位缓存。
    /// 跳过帧也要推进采样账（监控历史随快照节奏入账、窗口显隐不影响记账）。
    /// 返回本帧是否刚刷新快照（面板 pass 触发源之一）。
    pub(in crate::overlay) fn tick_snapshot(&mut self, delta_secs: f32) -> bool {
        let ticked = self.fps.tick(delta_secs);
        if ticked {
            self.history
                .sample(self.fps.snapshot.fps, self.fps.snapshot.ms, self.vulkan.as_deref());
            // 分位数面板缓存同节奏重算（调试窗逐帧只读，不再逐帧全量 sort）
            self.pct_cache.refresh(&self.perf);
        }
        ticked
    }

    /// 切页折叠/点选/树形态变化（资源 Changed）：面板 pass 触发源之一。
    pub(in crate::overlay) fn changed(&self) -> bool {
        self.tool_state.is_changed() || self.selected.is_changed() || self.tree_state.is_changed()
    }

    /// 建工具窗口并按当前页分发内容（须在 begin/end pass 之间调用）；实体树页
    /// 的 Transform 拖改落进编辑队列。
    pub(in crate::overlay) fn show(&mut self, ctx: &egui::Context, window: &Window, ppp: f32) {
        let mut edit = None;
        ToolWindow { ctx, state: &mut self.tool_state }.show(|body, page| match page {
            ToolPage::Shading => ShadingPage { mode: &mut self.mode }.show(body),
            ToolPage::Culling => CullingPage {
                cull_settings: &mut self.cull_settings,
                cull_diag: &self.cull_diag,
                occlusion_settings: &mut self.occlusion_settings,
                occlusion_diag: &self.occlusion_diag,
                lod_settings: &mut self.lod_settings,
                lod_diag: &self.lod_diag,
                indirect_stats: &self.indirect_stats,
            }
            .show(body),
            ToolPage::Pipeline => PipelinePage {
                shadow: &mut self.shadow_settings,
                bloom: &mut self.bloom_settings,
                smaa: &mut self.smaa_settings,
            }
            .show(body),
            ToolPage::FrameStats => FrameStatsPage {
                fps: self.fps.snapshot,
                window,
                ppp,
                stats: &self.stats,
                pct: &self.pct_cache,
                workload: &self.workload,
                image_cache: &self.image_cache,
                mesh_pool: &self.mesh_pool,
                chunks: &self.chunks,
            }
            .show(body),
            ToolPage::Monitor => MonitorPage {
                history: &self.history,
                fps: self.fps.snapshot,
                mesh_pool: &self.mesh_pool,
                image_cache: &self.image_cache,
                frame_sync: &mut self.frame_sync,
            }
            .show(body),
            ToolPage::EntityTree => {
                edit = EntityTreePage {
                    selected: &mut self.selected,
                    state: &mut self.tree_state,
                    details: &self.details,
                    drag: &mut self.drag_cache,
                }
                .show(body);
            }
        });
        if let Some(edit) = edit {
            self.edit_queue.0.push(edit);
        }
    }
}
