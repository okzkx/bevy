//! egui 框架半边：Startup 建状态，Update 里按**节拍**跑 egui pass（纯 CPU），
//! 产 [`EguiFrame`] 交绘制半边。
//!
//! overlay 按内容分两半：本文件是框架半边——"怎么跑"（Context/字体、pass、
//! 纹理增量折叠、帧产出）；窗口内容"画什么"是业务，住
//! [`crate::overlay::windows`]（本文件的 pass
//! 只负责按总控开关在 begin/end 之间调它们）。
//!
//! 节拍化（7.8）：pass 分两层——面板层（总控/监控/统计/树）由输入事件、
//! 0.5s 快照、窗口开闭等触发；gizmo 层（坐标轴指示器）由相机旋转驱动。其余
//! 帧不跑 pass，`EguiFrame` 两层代数保持，绘制半边按代数命中镶嵌缓存
//! （官方集成 eframe 的"空闲不重绘"在本宿主的等价物——本宿主每帧 present，
//! 等价物落在 shapes 供给与镶嵌缓存两侧）。
//!
//! pass 四件（egui 0.36）：`begin_pass(RawInput)` →
//! 插件逐帧账本（on_begin_pass/on_end_pass，[`plugin_hooks_begin`] 注释）→
//! 建窗口 UI → `end_pass() -> FullOutput`。`FullOutput.shapes` 是**未镶嵌**的
//! 原始形状——镶嵌（`ctx.tessellate`）归绘制半边在 `draw_frame` 内做（写顶点环
//! 须过 `wait_for_slot` 的 fence）。
//!
//! 字体：egui `default_fonts`（拉丁字形）+ Windows 系统微软雅黑
//! （`C:\Windows\Fonts\msyh.ttc`，face index 0）插进 Proportional 回退位——拉丁
//! 仍走自带 Ubuntu，CJK 落到雅黑。读不到 warn 后继续（Tier①：调试 UI 缺 CJK
//! 显示为豆腐块，不影响帧循环）。

use std::sync::Arc;

use bevy::{
    camera::Camera,
    ecs::system::SystemParam,
    input::{
        keyboard::{KeyCode, KeyboardInput},
        mouse::{MouseButtonInput, MouseWheel},
        ButtonInput,
    },
    prelude::*,
    window::{CursorLeft, CursorMoved, PrimaryWindow, Window, WindowFocused},
};

use super::input::{egui_raw_input, EguiInput};
use super::paint::{AtlasGpu, AtlasMirror};
use crate::driver::host::FrameSyncSettings;
use crate::overlay::transform_edit::{TransformEditQueue, apply_transform_edits};
use crate::overlay::widgets::axis_gizmo::AxisGizmo;
use crate::overlay::widgets::load_popup::{load_activity_tick, AssetLoadPopup, LoadActivity};
use crate::overlay::windows::debug_hub_window::{DebugHubWindow, DebugWindowsOpen};
use crate::overlay::windows::debug_window::{
    ChunkRows, DebugWindow, FpsMeter, PerfPanelCache, RenderMode, RendererStats,
};
use crate::overlay::windows::entity_tree_window::{
    EntityTreeState, EntityTreeWindow, SelectedDetails, SelectedEntity, TransformDragCache,
    collect_selected_details, maintain_entity_tree, mark_tree_dirty_despawn,
    mark_tree_dirty_insert, mark_tree_dirty_remove,
};
use crate::overlay::windows::monitor_window::{MonitorHistory, MonitorWindow};
use crate::scene::mechanism::WorkloadSettings;
use crate::vulkan::{
    CullDiagnostics, CullSettings, Context, ImageCache, IndirectStats, MeshPool, LodDiagnostics, LodSettings, OcclusionDiagnostics,
    OcclusionSettings, PerfLedger,
};

use ash_macros::system;

/// 调试 UI 插件：Startup 建状态，Update 跑 egui pass。绘制半边（图集/顶点/管线）
/// 在 `driver`/`vulkan` 侧，消费本插件产的 [`EguiFrame`]。
pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, init_egui)
            .init_resource::<EguiInput>()
            // 树拍平脏信号（7.7）：任一组件 Insert/Remove/Despawn 都可能改树
            // 形态——观测器只置脏，重建归维护系统统一做（稳态零成本）
            .add_observer(mark_tree_dirty_insert)
            .add_observer(mark_tree_dirty_remove)
            .add_observer(mark_tree_dirty_despawn)
            // 属性链：树拍平维护（脏才重建）→ 采集（产快照）→ egui pass
            //（显示 + 组编辑）→ 编辑清账，链式定序；清账完 PostUpdate 传播
            // 才读 Transform，同帧 relay 保画面跟随
            .add_systems(
                Update,
                (
                    maintain_entity_tree,
                    collect_selected_details,
                    load_activity_tick,
                    run_egui_pass,
                    apply_transform_edits,
                )
                    .chain(),
            );
    }
}

/// egui 全局状态：`Context`（进程单例语义，Arc 内核可克隆）+ CJK 字体。
#[derive(Resource)]
pub struct EguiState {
    pub ctx: egui::Context,
}

/// 一帧 egui 输出（Update 产，`draw_frame` 内绘制半边消费）。
#[derive(Resource, Default)]
pub struct EguiFrame {
    /// 面板层（总控/监控/统计/实体树，输入与快照节拍驱动）。
    pub panel: EguiLayer,
    /// gizmo 层（坐标轴指示器，相机旋转驱动独立节拍）。
    pub gizmo: EguiLayer,
    /// 本帧 pixels_per_point（= 窗口 scale_factor）。
    pub pixels_per_point: f32,
    /// 屏幕 points 尺寸（scissor 换算的分母域）。
    pub screen_points: Vec2,
}

/// 一层 egui 产出（7.8 节拍化）：shapes 仅在**本层 pass 运行帧**填充（下一个
/// 绘制帧被 paint_overlay 取走镶嵌），generation 只在本层 pass 重跑时 +1——
/// 跳过帧绘制半边按代数命中镶嵌缓存（零重求值零重镶嵌，画面静止即正确）。
/// 官方集成（eframe）靠"空闲不重绘、窗口保留上一帧"拿到同样效果；本宿主
/// 每帧 present，等价物落在 shapes 供给与镶嵌缓存两侧。
#[derive(Default)]
pub struct EguiLayer {
    /// 未镶嵌的原始形状（左上原点 points 域）；跳过帧为空（已被取走或未跑）。
    pub shapes: Vec<egui::epaint::ClippedShape>,
    /// 本层代数：本层 pass 重跑 +1。绘制半边据此判定镶嵌缓存命中。
    pub generation: u64,
}

/// egui pass 自计时（7.5）：最近一次 pass 的墙钟耗时——Update 侧在
/// draw_frame 计量之外，FPS 恶化取证的 Update 半边仪器。
#[derive(Resource, Default)]
pub struct EguiPassTiming {
    /// 最近一次 run_egui_pass 的墙钟毫秒（两层 pass 合计；跳过帧为 0）。
    pub last_ms: f32,
}

/// Startup：建 egui Context + 装字体 + 插状态资源。
#[system]
fn init_egui(mut commands: Commands) {
    let state = EguiState {
        ctx: egui::Context::default(),
    };
    state.ctx.set_fonts(load_fonts());
    // 跨标签文字选区（egui 0.36 默认开）允许选区端点在标签间迁移——拖选扫过多个
    // 标签时杂染面更大：调试面板选区限制在单标签内。单标签内的无按键
    // 悬停重投影由 pass 内补跑的插件生命周期收口（run_egui_pass）。
    let mut style = (*state.ctx.style_of(egui::Theme::Dark)).clone();
    style.interaction.multi_widget_text_select = false;
    state.ctx.set_style_of(egui::Theme::Dark, style);
    commands.insert_resource(state);
    commands.insert_resource(EguiFrame::default());
    commands.insert_resource(RenderMode::from_env());
    // 各调试窗口显隐总控：总控面板写、pass 显示门读，默认全开
    commands.insert_resource(DebugWindowsOpen::default());
    // 层级树点选：属性面板的编辑目标来源
    commands.insert_resource(SelectedEntity::default());
    // 树拍平缓存（7.7）：脏信号观测器 + 维护系统重建，默认脏 = 首帧立树
    commands.insert_resource(EntityTreeState::default());
    // 属性快照：采集系统每帧重写，Startup 先落空壳
    commands.insert_resource(SelectedDetails::default());
    // 旋转拖拽缓存：拖拽期内冻结欧拉表示分支（防跨分支混写闪烁）
    commands.insert_resource(TransformDragCache::default());
    // Transform 编辑队列：pass 产、清账系统消费
    commands.insert_resource(TransformEditQueue::default());
    // 帧率显示平滑：0.5s 出一次平均快照
    commands.insert_resource(FpsMeter::default());
    // 持续监控历史（7.5）：随 fps 快照节奏采样，窗口显隐不影响记账
    commands.insert_resource(MonitorHistory::default());
    // egui pass 自计时（7.5）：Update 侧墙钟读数
    commands.insert_resource(EguiPassTiming::default());
    // 分位数面板缓存（7.5）：0.5s 重算，调试窗只读
    commands.insert_resource(PerfPanelCache::default());
    // 资产加载活跃 latch（7.12）：默认 active（覆盖 Startup 解析窗口）
    commands.insert_resource(LoadActivity::default());
    // 图集镜像（Update 折入）与 GPU 代（Last 整传；空建——首帧有整图增量才落图）。
    // AtlasGpu 的拆除在 teardown_vulkan（graveyard 与表同寿的拆除序）
    commands.insert_resource(AtlasMirror::default());
    commands.insert_resource(AtlasGpu::default());
    info!("egui 状态就绪：Context + 字体（default_fonts + msyh 回退），pass 自下一帧（Update）");
}

/// 默认字体 + 微软雅黑 CJK 回退。msyh 缺席时返回原样（Tier① warn）。
fn load_fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    const MSYH: &str = r"C:\Windows\Fonts\msyh.ttc";
    match std::fs::read(MSYH) {
        Ok(bytes) => {
            // ttc face 0 = 常规体雅黑；插 Proportional 第二位（拉丁仍走 Ubuntu，
            // CJK 回退雅黑）+ Monospace 尾位（等宽里 CJK 也有字形可落）
            fonts
                .font_data
                .insert("msyh".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(1, "msyh".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("msyh".into());
        }
        Err(e) => {
            bevy::log::warn!("读取 {MSYH} 失败（{e}）：调试 UI 的 CJK 将显示为豆腐块，其余照常");
        }
    }
    fonts
}

/// 各调试窗口面板的一次性参数束（ui.rs 接线层）：统计、着色模式、显隐总控、
/// 点选与层级树查询合成一个 SystemParam——系统参数上限 16（FrameInput 同款式
/// 的束打法），新增面板件全走这里，不撑大 pass 签名。
#[derive(SystemParam)]
struct DebugUiParams<'w, 's> {
    stats: RendererStats<'w>,
    mode: ResMut<'w, RenderMode>,
    windows_open: ResMut<'w, DebugWindowsOpen>,
    selected: ResMut<'w, SelectedEntity>,
    fps: ResMut<'w, FpsMeter>,
    /// egui pass 自计时（7.5）
    timing: ResMut<'w, EguiPassTiming>,
    /// 分位数面板缓存（7.5）：0.5s 重算
    pct_cache: ResMut<'w, PerfPanelCache>,
    /// 持续监控历史（7.5）：fps 快照刷新时采样入账
    history: ResMut<'w, MonitorHistory>,
    /// Vulkan 上下文（7.5 显存堆级读数；init 前空帧缺席）
    vulkan: Option<Res<'w, Context>>,
    /// 树拍平缓存（7.7）：维护系统产，pass 只读；展开/收起写回
    tree_state: ResMut<'w, EntityTreeState>,
    /// 属性快照（采集系统产，只读消费）
    details: Res<'w, SelectedDetails>,
    /// 旋转拖拽缓存（拖拽期内冻结欧拉表示分支）
    drag_cache: ResMut<'w, TransformDragCache>,
    /// 主相机姿态（坐标轴指示器读旋转；本工程单相机，取任一）
    cameras: Query<'w, 's, &'static GlobalTransform, With<Camera>>,
    /// Transform 编辑排队（pass 产，清账系统消费）
    edit_queue: ResMut<'w, TransformEditQueue>,
    /// 剔除开关（5.2：checkbox 写，draw_frame 读）
    cull_settings: ResMut<'w, CullSettings>,
    /// 剔除诊断读数（draw_frame 每帧刷新，只读展示）
    cull_diag: Res<'w, CullDiagnostics>,
    /// 遮挡开关（6.1：checkbox 写，draw_frame 读）
    occlusion_settings: ResMut<'w, OcclusionSettings>,
    /// 遮挡诊断读数（6.1：draw_frame 每帧刷新，只读展示）
    occlusion_diag: Res<'w, OcclusionDiagnostics>,
    /// LOD 开关（6.2：checkbox 写，draw_frame 读）
    lod_settings: ResMut<'w, LodSettings>,
    /// LOD 诊断读数（6.2：draw_frame 每帧刷新，只读展示）
    lod_diag: Res<'w, LodDiagnostics>,
    /// indirect 闭环读数（5.3：draw_frame 每帧刷新，只读展示）
    indirect_stats: Res<'w, IndirectStats>,
    /// chunk 盘点（5.4：块数与成员总数计量行）
    chunks: ChunkRows<'w, 's>,
    /// 帧样本账（5.5：分位数/显存/注入读数）
    perf: Res<'w, PerfLedger>,
    /// 帧率同步开关与刷新率读数（7.6：checkbox 写，apply_frame_sync 读）
    frame_sync: ResMut<'w, FrameSyncSettings>,
    image_cache: Res<'w, ImageCache>,
    mesh_pool: Res<'w, MeshPool>,
    workload: Res<'w, WorkloadSettings>,
    /// 资产加载活跃 latch（7.12：菊花弹窗显示与面板直跑触发源）
    load_activity: Res<'w, LoadActivity>,
}

/// 节拍化的跨帧门控账（7.8）：pass 触发判定的全部 Local 状态收拢成束——
/// 系统参数上限 16（DebugUiParams 同款束打法）。
#[derive(SystemParam)]
struct PassGate<'w, 's> {
    /// SystemParam 束的双生命周期占位（字段全走 's，'w 由 derive 约定要求）
    _p: std::marker::PhantomData<&'w ()>,
    /// 收账日志去重：首个非空 shapes 的一次性收账
    first_shapes_logged: Local<'s, bool>,
    /// 面板/gizmo pass 是否至少跑过一次（首帧强制，立 UI）
    pass_ever_ran: Local<'s, bool>,
    /// 指针拖拽是否始于 UI 层（拖拽全程面板逐帧重跑，拖出窗外不冻结）
    ui_drag: Local<'s, bool>,
    /// 窗口尺寸/ppp 快照（[宽, 高, ppp]；变化 → 面板与 gizmo 都重跑）
    last_size: Local<'s, [f32; 3]>,
    /// gizmo 上次绘制时读到的相机旋转（None = 相机缺席）
    last_cam_rot: Local<'s, Option<Option<Quat>>>,
    /// gizmo 显隐上次快照
    last_gizmo_open: Local<'s, bool>,
    /// 面板 pass 跳过帧标记（门控翻转帧补发一次当前位置）
    skipped_since_pass: Local<'s, bool>,
    /// egui 自身请求的立即重绘（动画/悬停渐变/光标闪烁）→ 下帧强制重跑
    force_next: Local<'s, bool>,
    /// 最近一次面板 pass 画过的层（gizmo 子 pass 据此补记可见性，防淡入重置）
    panel_layers: Local<'s, Vec<egui::LayerId>>,
}

/// Update：组 RawInput（每帧排水，桥簿记无条件）→ 触发判定 → 面板 pass
///（输入/快照/开关等触发）与 gizmo pass（相机旋转驱动）各自成对
/// begin/end → 落 [`EguiFrame`] 两层。跳过帧不跑 pass，`EguiFrame` 代数
/// 保持、shapes 由绘制半边的镶嵌缓存接住（画面静止即正确）。
///
/// 触发集（面板层）：首帧 ∨ fps 快照到期（0.5s）∨ 窗口尺寸/ppp 变化 ∨
/// egui 自身请求立即重绘 ∨ 窗口开闭/点选/树形态变化（资源 Changed）∨
/// 面板相关输入事件（键/滚轮/焦点/按键；指针移动仅当悬于 UI 层或拖拽始于
/// UI——纯场景上的移动是漫游环视，面板静止不值得重跑）。gizmo 层：首帧 ∨
/// 尺寸变化 ∨ 显隐变化 ∨ 相机旋转变化（gizmo 只表达旋转，平移不重画）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：六个事件读取器 + 窗口/时间/按键 + egui 状态帧与图集镜像资源 + 调试窗口面板束 + 门控账束，逐项声明是框架惯例"
)]
#[system]
fn run_egui_pass(
    state: ResMut<EguiState>,
    mut frame: ResMut<EguiFrame>,
    mut input: ResMut<EguiInput>,
    mut mirror: ResMut<AtlasMirror>,
    mut ui: DebugUiParams,
    mut gate: PassGate,
    window: Query<&Window, With<PrimaryWindow>>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut keyboard: MessageReader<KeyboardInput>,
    mut mouse_button: MessageReader<MouseButtonInput>,
    mut wheel: MessageReader<MouseWheel>,
    mut cursor: MessageReader<CursorMoved>,
    mut cursor_left: MessageReader<CursorLeft>,
    mut window_focused: MessageReader<WindowFocused>,
) {
    let Some(window) = window.iter().next() else {
        return; // 主窗未建（Startup 前的空帧）：egui 无屏幕域，整帧让路
    };
    // —— 节拍底座（每帧，不隶属 pass）：fps 快照 + 监控采样 + 分位缓存。
    // pass 节拍化后不能躺在 pass 里——跳过帧也要推进采样账（监控历史随
    // 快照节奏入账、窗口显隐不影响记账的原语义保持）。
    let snapshot_ticked = ui.fps.tick(time.delta_secs());
    if snapshot_ticked {
        ui.history
            .sample(ui.fps.snapshot.fps, ui.fps.snapshot.ms, ui.vulkan.as_deref());
        // 分位数面板缓存同节奏重算（调试窗逐帧只读，不再逐帧全量 sort）
        ui.pct_cache.refresh(&ui.perf);
    }
    let pass_clock = std::time::Instant::now();
    let ppp = window.scale_factor();
    let screen_points = Vec2::new(window.width(), window.height());
    // 事件排水 + 桥簿记每帧无条件（读取器排空是 bevy Messages 双缓冲纪律；
    // 面板 pass 不跑时事件由桥积压，下次 pass 合并喂入）
    let mut raw =
        egui_raw_input(
            window, &time, &keys, &mut input, &mut keyboard, &mut mouse_button, &mut wheel,
            &mut cursor, &mut cursor_left, &mut window_focused,
        );
    let raw_rect = raw.screen_rect;
    let raw_time = raw.time;
    let raw_focused = raw.focused;
    // —— 触发判定 ——
    let size = [window.width(), window.height(), ppp];
    let size_changed = *gate.last_size != size;
    let cam_rot = ui.cameras.iter().next().map(|g| g.rotation());
    let cam_rot_changed = *gate.last_cam_rot != Some(cam_rot);
    *gate.last_cam_rot = Some(cam_rot);
    // 指针在面板层上（浮动窗口层；gizmo 层纯显示无悬停态，不计入——悬停
    // gizmo 不值得重跑面板）。与 camera_control 的仲裁同式（layer_id_at）。
    let over_ui = |pos: egui::Pos2| {
        state
            .ctx
            .layer_id_at(pos)
            .is_some_and(|layer| layer.order != egui::Order::Background)
    };
    let mut panel_events = false;
    for ev in &raw.events {
        match ev {
            egui::Event::PointerMoved(pos) => {
                if over_ui(*pos) {
                    panel_events = true;
                }
            }
            egui::Event::PointerButton {
                pos, pressed: true, ..
            } => {
                // 拖拽起点在面板层上 → 拖拽全程面板逐帧重跑（点选/拖选/拖窗
                // 跟手；拖出窗外不冻结，松手即清）
                *gate.ui_drag = over_ui(*pos);
                panel_events = true;
            }
            _ => panel_events = true,
        }
    }
    if !input.any_button_down() {
        *gate.ui_drag = false;
    }
    let panel_due = !*gate.pass_ever_ran
        || snapshot_ticked
        || size_changed
        || *gate.force_next
        // 加载期直跑（7.12）：菊花相位逐帧推进（0.5s 快照节拍会看着卡顿）；
        // 加载完成弹窗隐藏后触发自然消失回落节拍
        || ui.load_activity.active
        || ui.windows_open.is_changed()
        || ui.selected.is_changed()
        || ui.tree_state.is_changed()
        || panel_events;
    let gizmo_due =
        !*gate.pass_ever_ran || size_changed || ui.windows_open.axis_gizmo != *gate.last_gizmo_open || cam_rot_changed;
    *gate.force_next = false;

    if panel_due {
        // 积压合并（旧事件在前）+ 门控翻转帧补发一次当前位置：进面板建
        // 悬停、出面板清悬停，缺了悬停高亮冻结在旧形状里（7.8 计划钉子）
        let mut events = input.drain_pending();
        events.append(&mut raw.events);
        if *gate.skipped_since_pass
            && let Some(pos) = input.pointer_pos()
        {
            events.push(egui::Event::PointerMoved(pos));
        }
        *gate.skipped_since_pass = false;
        raw.events = events;
        state.ctx.set_pixels_per_point(ppp);
        state.ctx.begin_pass(raw);
        let plugin_host = plugin_hooks_begin(&state);
        // 总控先行（自身不可关），其余窗口按开关显隐；[×] 与 checkbox 写同一字段
        DebugHubWindow { ctx: &state.ctx, open: &mut ui.windows_open }.show();
        if ui.windows_open.monitor {
            MonitorWindow {
                ctx: &state.ctx,
                open: &mut ui.windows_open.monitor,
                history: &ui.history,
                fps: ui.fps.snapshot,
                mesh_pool: &ui.mesh_pool,
                image_cache: &ui.image_cache,
                frame_sync: &mut ui.frame_sync,
            }
            .show();
        }
        if ui.windows_open.stats {
            DebugWindow {
                ctx: &state.ctx,
                open: &mut ui.windows_open.stats,
                fps: ui.fps.snapshot,
                window,
                ppp,
                mode: &mut ui.mode,
                stats: &ui.stats,
                cull_settings: &mut ui.cull_settings,
                cull_diag: &ui.cull_diag,
                occlusion_settings: &mut ui.occlusion_settings,
                occlusion_diag: &ui.occlusion_diag,
                lod_settings: &mut ui.lod_settings,
                lod_diag: &ui.lod_diag,
                indirect_stats: &ui.indirect_stats,
                pct: &ui.pct_cache,
                workload: &ui.workload,
                image_cache: &ui.image_cache,
                mesh_pool: &ui.mesh_pool,
                chunks: &ui.chunks,
            }
            .show();
        }
        if ui.windows_open.entity_tree {
            let edit = EntityTreeWindow {
                ctx: &state.ctx,
                open: &mut ui.windows_open.entity_tree,
                selected: &mut ui.selected,
                state: &mut ui.tree_state,
                details: &ui.details,
                drag: &mut ui.drag_cache,
            }
            .show();
            if let Some(edit) = edit {
                ui.edit_queue.0.push(edit);
            }
        }
        // 加载指示弹窗（7.12）：最后画（右下角浮在窗区之上）；Background 层
        // 纯显示零交互，不进 panel_layers 之外的仲裁面
        if ui.load_activity.active {
            AssetLoadPopup { ctx: &state.ctx }.show();
        }
        let egui::FullOutput {
            shapes,
            mut textures_delta,
            viewport_output,
            ..
        } = plugin_hooks_end(&state, plugin_host);
        mirror.fold(&mut textures_delta);
        frame.panel.shapes = shapes;
        frame.panel.generation += 1;
        // 本 pass 画过的层清单：gizmo 子 pass 期间用它补记可见性（见 gizmo 分支）
        *gate.panel_layers = state
            .ctx
            .memory(|m| m.areas().visible_layer_ids())
            .into_iter()
            .collect();
        // egui 自管的重绘请求（动画/悬停渐变/文本光标闪烁）：delay == 0 即
        // "下帧立刻要重画"——官方集成同款消费，否则动画在跳过帧里冻结
        *gate.force_next = viewport_output
            .get(&state.ctx.viewport_id())
            .is_some_and(|vp| vp.repaint_delay == core::time::Duration::ZERO);
        *gate.pass_ever_ran = true;
    } else if !raw.events.is_empty() {
        // 面板跳过帧：事件积压（簿记已更新、读取器已排空，只欠"喂给 egui"）
        input.stash_events(std::mem::take(&mut raw.events));
        *gate.skipped_since_pass = true;
    }
    if gizmo_due {
        if ui.windows_open.axis_gizmo {
            // gizmo 纯显示零交互：空事件子 pass（指针/按键状态由 egui 跨
            // pass 自持，不重复喂积压——事件只消费一次，归面板 pass）
            state.ctx.set_pixels_per_point(ppp);
            state.ctx.begin_pass(egui::RawInput {
                screen_rect: raw_rect,
                time: raw_time,
                focused: raw_focused,
                ..Default::default()
            });
            // 面板层可见性补记（7.8 面板消失修复）：egui 按"上一个 pass 画过谁"
            // 记账窗口可见性（Areas::end_pass 换道 visible_areas_last_frame）——
            // 本子 pass 不画任何窗口，若不补记，面板层会从账面蒸发，下个面板
            // pass 被 egui 视为"窗口刚重新出现"而重触发淡入动画（Area::content_ui
            // 的 multiply_opacity，~0.08s 爬升），该帧 shapes 带低 alpha 进冻结
            // 缓存——表现即"相机移动中面板消失只剩阴影，停下后几十毫秒恢复"。
            // move_to_top 顺带置 wants_to_be_on_top，但同 Order 桶内稳定排序不
            // 改变面板间相对层序（副作用无害）。
            state.ctx.memory_mut(|m| {
                for layer in gate.panel_layers.iter() {
                    m.areas_mut().move_to_top(*layer);
                }
            });
            let plugin_host = plugin_hooks_begin(&state);
            AxisGizmo {
                ctx: &state.ctx,
                cam_rotation: cam_rot,
            }
            .show();
            let egui::FullOutput {
                shapes,
                mut textures_delta,
                ..
            } = plugin_hooks_end(&state, plugin_host);
            mirror.fold(&mut textures_delta);
            frame.gizmo.shapes = shapes;
        } else {
            // 显隐关：shapes 清空（代数照常推进，绘制半边本层产空 → 无 gizmo）
            frame.gizmo.shapes.clear();
        }
        frame.gizmo.generation += 1;
        *gate.last_gizmo_open = ui.windows_open.axis_gizmo;
        *gate.pass_ever_ran = true;
    }
    *gate.last_size = size;
    frame.pixels_per_point = ppp;
    frame.screen_points = screen_points;
    // pass 自计时落账（7.5）：两层合计；跳过帧为 0（监控读数如实反映节拍）
    ui.timing.last_ms = if *gate.pass_ever_ran && (panel_due || gizmo_due) {
        pass_clock.elapsed().as_secs_f32() * 1000.0
    } else {
        0.0
    };
    if !*gate.first_shapes_logged
        && (!frame.panel.shapes.is_empty() || !frame.gizmo.shapes.is_empty())
    {
        *gate.first_shapes_logged = true;
        info!(
            "egui pass 收账：shapes 面板 {} + gizmo {}，ppp {ppp}，屏 {:.0}×{:.0}pt",
            frame.panel.shapes.len(),
            frame.gizmo.shapes.len(),
            frame.screen_points.x,
            frame.screen_points.y,
        );
    }
}

/// 裸集成必补的插件生命周期（egui 默认插件的 on_begin_pass/on_end_pass 只在
/// run_ui 闭包 API（run_ui_dyn）里被驱动，begin_pass/end_pass 裸集成必须自己
/// 补——否则 LabelSelectionState 的 is_dragging 首次拖选后永不清零，选区随
/// 无按键悬停逐帧重投影；multi_widget_text_select=false 只封跨标签半边，
/// 单标签内的悬停重投影由此收口）。空 root Ui 仅作钩子宿主（同 run_ui_dyn
/// 形状）；run_ui 的 root_ui_available_rect 副产物不补（layer_id_at 已够用）。
/// 其余默认插件不补：CallbackPlugin 零注册即 no-op（且类型不可名）、
/// DragAndDrop/DebugTextPlugin 钩子在本宿主零负载（无 DnD payload/无 debug
/// text），将来用到时照此各加一段。7.8 节拍化：面板 pass 与 gizmo 子 pass
/// 各自完整配对（跳过帧整个 pass 不存在，无钩子可配）。
fn plugin_hooks_begin(state: &EguiState) -> egui::Ui {
    let mut plugin_host = egui::Ui::new(
        state.ctx.clone(),
        egui::Id::new((state.ctx.viewport_id(), "__top_ui")),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(state.ctx.viewport_rect()),
    );
    if let Some(label_selection) = state
        .ctx
        .plugin_opt::<egui::text_selection::LabelSelectionState>()
    {
        let mut plugin = label_selection.lock();
        egui::Plugin::on_begin_pass(&mut *plugin, &mut plugin_host);
    }
    plugin_host
}

fn plugin_hooks_end(state: &EguiState, mut plugin_host: egui::Ui) -> egui::FullOutput {
    if let Some(label_selection) = state
        .ctx
        .plugin_opt::<egui::text_selection::LabelSelectionState>()
    {
        let mut plugin = label_selection.lock();
        egui::Plugin::on_end_pass(&mut *plugin, &mut plugin_host);
    }
    state.ctx.end_pass()
}
