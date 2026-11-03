//! egui 框架："怎么跑"——Startup 建状态（Context/字体），Update 里按节拍跑
//! egui pass（纯 CPU）、折入纹理增量，产 [`EguiFrame`] 交绘制段；窗口内容
//! "画什么"在 [`crate::overlay::tool_window`]（pass 只在 begin/end 之间调它的 `show`）。
//!
//! 节拍：pass 分两层——面板层（工具窗口）由输入事件、0.5s 快照、切页/折叠等
//! 触发；gizmo 层（坐标轴指示器）由相机旋转驱动。其余帧不跑 pass，`EguiFrame`
//! 两层代数保持，绘制段按代数命中镶嵌缓存。
//!
//! pass 形状：`begin_pass(RawInput)` → 插件钩子（[`plugin_hooks_begin`]）→ 建窗口
//! UI → `end_pass() -> FullOutput`。`FullOutput.shapes` 未镶嵌，镶嵌归绘制段在
//! `draw_frame` 内做（写顶点环须过 `wait_for_slot` 的 fence）。
//!
//! 字体：egui 默认字体 + Windows 微软雅黑（`msyh.ttc`）作 CJK 回退；读不到 warn 后
//! 继续（CJK 显示为豆腐块，不影响帧循环）。

use std::sync::Arc;

use bevy::{
    camera::Camera,
    ecs::system::SystemParam,
    prelude::*,
    window::{PrimaryWindow, Window},
};

use super::input::{EguiInput, InputBridge};
use super::paint::AtlasMirror;
use crate::common::syntax::{unwrap_or_return, OnceLog};
use crate::render::{AtlasGpu, EguiPassTiming};
use crate::scene::{
    apply_transform_edits, PointerCapture, PointerCaptureSet, TransformEditQueue,
};
use crate::overlay::widgets::axis_gizmo::AxisGizmo;
use crate::overlay::widgets::load_popup::{load_activity_tick, AssetLoadPopup, LoadActivity};
use crate::overlay::tool_window::entity_tree::{
    add_tree_dirty_observers, collect_selected_details, maintain_entity_tree,
};
use crate::overlay::tool_window::{insert_tool_window_resources, ToolWindowParams};

use ash_macros::system;

/// 调试 UI 插件：Startup 建状态，Update 跑 egui pass 并发布输入仲裁。绘制段
///（`paint_overlay`）由 `draw_frame` 调用，消费本插件产的 [`EguiFrame`]。
pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, init_egui)
            .init_resource::<EguiInput>()
            // 输入仲裁：每帧（不受 pass 节拍门控）发布，相机/日光控制排在其后读
            .init_resource::<PointerCapture>()
            .add_systems(Update, publish_pointer_capture.in_set(PointerCaptureSet));
        add_tree_dirty_observers(app)
            // 链式定序：树维护（脏才重建）→ 属性快照采集 → 加载判据 → egui pass
            //（显示 + Transform 编辑排队）→ 编辑落地；落地后 PostUpdate 传播才读
            // Transform，同帧画面跟随
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

/// egui 全局状态：`Context`（Arc 内核，克隆共享同一份）。
#[derive(Resource)]
pub struct EguiState {
    pub ctx: egui::Context,
}

/// 一帧 egui 输出（Update 产，`draw_frame` 内绘制段消费）。
#[derive(Resource, Default)]
pub struct EguiFrame {
    /// 面板层（工具窗口，输入与快照节拍驱动）。
    pub panel: EguiLayer,
    /// gizmo 层（坐标轴指示器，相机旋转驱动独立节拍）。
    pub gizmo: EguiLayer,
    /// 本帧 pixels_per_point（= 窗口 scale_factor）。
    pub pixels_per_point: f32,
    /// 屏幕 points 尺寸（scissor 换算的分母域）。
    pub screen_points: Vec2,
}

/// 一层 egui 产出：shapes 仅在本层 pass 运行帧填充（下一个绘制帧被
/// paint_overlay 取走镶嵌），generation 只在本层 pass 重跑时 +1——跳过帧绘制
/// 段按代数复用镶嵌缓存（本宿主每帧 present，靠缓存而非"空闲不重绘"保画面）。
#[derive(Default)]
pub struct EguiLayer {
    /// 未镶嵌的原始形状（左上原点 points 域）；跳过帧为空（已被取走或未跑）。
    pub shapes: Vec<egui::epaint::ClippedShape>,
    /// 本层代数：本层 pass 重跑 +1。绘制段据此判定镶嵌缓存命中。
    pub generation: u64,
}

/// 指针是否悬在 UI 浮动层上：查该点最顶层，非 Background 层 = 调试面板。
///
/// 坑：不用 `is_pointer_over_egui()`——它依赖 `run_ui` 闭包 API 写入的
/// `root_ui_available_rect`，本集成的 `begin_pass`/`end_pass` 形状下该字段恒
/// None，指针在背景上时恒返回 true（实测门永远关死）。`layer_id_at` 读最近
/// 一次 pass 的层序，最多滞后一帧——点击瞬间指针不会瞬移，调试 UI 可接受。
fn pointer_over_ui(ctx: &egui::Context, pos: egui::Pos2) -> bool {
    ctx.layer_id_at(pos)
        .is_some_and(|layer| layer.order != egui::Order::Background)
}

/// Update（[`PointerCaptureSet`]）：按本帧光标发布输入仲裁——光标 points 域
/// 与 egui 同域直比；光标离窗/主窗缺席/egui 未就绪 = 不让。每帧整体覆写，
/// 不受 pass 节拍门控。
#[system]
fn publish_pointer_capture(
    state: Option<Res<EguiState>>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut capture: ResMut<PointerCapture>,
) {
    let cursor = window.single().ok().and_then(Window::cursor_position);
    *capture = match state {
        Some(state) => PointerCapture {
            over_ui: cursor.is_some_and(|pos| pointer_over_ui(&state.ctx, egui::pos2(pos.x, pos.y))),
            wants_keyboard: state.ctx.egui_wants_keyboard_input(),
        },
        None => PointerCapture::default(),
    };
}

/// Startup：建 egui Context + 装字体 + 插状态资源。
#[system]
fn init_egui(mut commands: Commands) {
    let state = EguiState {
        ctx: egui::Context::default(),
    };
    state.ctx.set_fonts(load_fonts());
    // 关跨标签文字选区：拖选扫过多个标签时选区乱跳，调试面板限制在单标签内
    //（单标签内的悬停重投影由 plugin_hooks_begin 收口）
    let mut style = (*state.ctx.style_of(egui::Theme::Dark)).clone();
    style.interaction.multi_widget_text_select = false;
    state.ctx.set_style_of(egui::Theme::Dark, style);
    commands.insert_resource(state);
    commands.insert_resource(EguiFrame::default());
    insert_tool_window_resources(&mut commands);
    commands.insert_resource(TransformEditQueue::default());
    commands.insert_resource(EguiPassTiming::default());
    // 默认 active：覆盖 Startup 后 glb 尚未发起请求的解析窗口
    commands.insert_resource(LoadActivity::default());
    // 图集镜像（Update 折入）与 GPU 代（Last 整传；空建，首个整图增量才落图）。
    // AtlasGpu 由 teardown_vulkan 拆除（旧代与描述符表同寿）
    commands.insert_resource(AtlasMirror::default());
    commands.insert_resource(AtlasGpu::default());
    info!("egui 状态就绪：Context + 字体（default_fonts + msyh 回退），pass 自下一帧（Update）");
}

/// 默认字体 + 微软雅黑 CJK 回退。msyh 缺席时返回原样（warn 一次）。
fn load_fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    const MSYH: &str = r"C:\Windows\Fonts\msyh.ttc";
    match std::fs::read(MSYH) {
        Ok(bytes) => {
            // ttc face 0 = 常规体雅黑；插 Proportional 第二位（拉丁仍走默认字体，
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
            warn!("读取 {MSYH} 失败（{e}）：调试 UI 的 CJK 将显示为豆腐块，其余照常");
        }
    }
    fonts
}

/// pass 的业务参数束：工具窗口参数束 + 框架侧消费的少量读源（自计时、
/// 相机旋转、加载 latch）——收成束避开系统参数 16 个上限。
#[derive(SystemParam)]
struct DebugUiParams<'w, 's> {
    /// 工具窗口各页读写的资源（页面分发见 [`ToolWindowParams::show`]）
    tools: ToolWindowParams<'w, 's>,
    timing: ResMut<'w, EguiPassTiming>,
    /// 主相机姿态（坐标轴指示器读旋转；本工程单相机，取任一）
    cameras: Query<'w, 's, &'static GlobalTransform, With<Camera>>,
    /// 资产加载活跃 latch（加载弹窗显示与面板直跑触发源）
    load_activity: Res<'w, LoadActivity>,
}

/// pass 触发判定的跨帧 Local 状态束。
#[derive(SystemParam)]
struct PassGate<'w, 's> {
    /// derive 要求的 'w 占位（字段全走 's）
    _p: std::marker::PhantomData<&'w ()>,
    /// 首个非空 shapes 的一次性收账日志
    first_shapes_logged: Local<'s, OnceLog>,
    /// 面板/gizmo pass 是否至少跑过一次（首帧强制，立 UI）
    pass_ever_ran: Local<'s, bool>,
    /// 指针拖拽是否始于 UI 层（拖拽全程面板逐帧重跑，拖出窗外不冻结）
    ui_drag: Local<'s, bool>,
    /// 窗口尺寸/ppp 快照（[宽, 高, ppp]；变化 → 面板与 gizmo 都重跑）
    last_size: Local<'s, [f32; 3]>,
    /// gizmo 上次绘制时读到的相机旋转（None = 相机缺席）
    last_cam_rot: Local<'s, Option<Option<Quat>>>,
    /// 面板 pass 跳过帧标记（门控翻转帧补发一次当前位置）
    skipped_since_pass: Local<'s, bool>,
    /// egui 自身请求的立即重绘（动画/悬停渐变/光标闪烁）→ 下帧强制重跑
    force_next: Local<'s, bool>,
    /// 最近一次面板 pass 画过的层（gizmo 子 pass 据此补记可见性，防淡入重置）
    panel_layers: Local<'s, Vec<egui::LayerId>>,
}

/// Update：组 RawInput（每帧排水，桥簿记无条件）→ 触发判定 → 面板 pass 与
/// gizmo pass 各自成对 begin/end → 落 [`EguiFrame`] 两层。跳过帧不跑 pass，
/// `EguiFrame` 代数保持、shapes 由绘制段的镶嵌缓存接住。
///
/// 面板层触发集：首帧 ∨ fps 快照到期（0.5s）∨ 窗口尺寸/ppp 变化 ∨ egui 请求立即
/// 重绘 ∨ 加载弹窗显示中 ∨ 切页折叠/点选/树形态变化 ∨ 面板相关输入（键/滚轮/焦点/
/// 按键；指针移动仅当悬于 UI 层或拖拽始于 UI——纯场景上的移动是漫游环视，面板
/// 不值得重跑）。gizmo 层：首帧 ∨ 尺寸变化 ∨ 相机旋转变化（平移不重画）。
#[system]
fn run_egui_pass(
    state: Res<EguiState>,
    mut frame: ResMut<EguiFrame>,
    mut mirror: ResMut<AtlasMirror>,
    mut ui: DebugUiParams,
    mut gate: PassGate,
    mut bridge: InputBridge,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    // 主窗未建（Startup 前的空帧）：egui 无屏幕域，整帧让路
    let window = unwrap_or_return!(window.iter().next());
    // 节拍底座（每帧，不隶属 pass）：跳过帧也要推进采样账
    let snapshot_ticked = ui.tools.tick_snapshot(bridge.time.delta_secs());
    let pass_clock = std::time::Instant::now();
    let ppp = window.scale_factor();
    let screen_points = Vec2::new(window.width(), window.height());
    // 事件排水 + 桥簿记每帧无条件；面板 pass 不跑时事件由桥积压，下次 pass 合并喂入
    let mut raw = bridge.raw_input(window);
    let raw_rect = raw.screen_rect;
    let raw_time = raw.time;
    let raw_focused = raw.focused;
    // —— 触发判定 ——
    let size = [window.width(), window.height(), ppp];
    let size_changed = *gate.last_size != size;
    let cam_rot = ui.cameras.iter().next().map(|g| g.rotation());
    let cam_rot_changed = *gate.last_cam_rot != Some(cam_rot);
    *gate.last_cam_rot = Some(cam_rot);
    // 指针在面板层上（gizmo 层纯显示无悬停态，不计入）；与输入仲裁同一判定
    let over_ui = |pos: egui::Pos2| pointer_over_ui(&state.ctx, pos);
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
                // 拖拽起点在面板层上 → 拖拽全程面板逐帧重跑（拖出窗外不冻结，松手即清）
                *gate.ui_drag = over_ui(*pos);
                panel_events = true;
            }
            _ => panel_events = true,
        }
    }
    if !bridge.state.any_button_down() {
        *gate.ui_drag = false;
    }
    let panel_due = !*gate.pass_ever_ran
        || snapshot_ticked
        || size_changed
        || *gate.force_next
        // 加载期直跑：菊花相位逐帧推进（0.5s 快照节拍会看着卡顿）
        || ui.load_activity.active
        || ui.tools.changed()
        || panel_events;
    let gizmo_due = !*gate.pass_ever_ran || size_changed || cam_rot_changed;
    *gate.force_next = false;

    if panel_due {
        // 积压合并（旧事件在前）+ 跳过后首帧补发一次当前位置：进面板建悬停、
        // 出面板清悬停，缺了悬停高亮冻结在旧形状里
        let mut events = bridge.state.drain_pending();
        events.append(&mut raw.events);
        if *gate.skipped_since_pass
            && let Some(pos) = bridge.state.pointer_pos()
        {
            events.push(egui::Event::PointerMoved(pos));
        }
        *gate.skipped_since_pass = false;
        raw.events = events;
        state.ctx.set_pixels_per_point(ppp);
        state.ctx.begin_pass(raw);
        let plugin_host = plugin_hooks_begin(&state);
        ui.tools.show(&state.ctx, window, ppp);
        // 加载弹窗最后画（右下角）；Background 层，不进指针仲裁
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
        // "下帧立刻要重画"，不消费则动画在跳过帧里冻结
        *gate.force_next = viewport_output
            .get(&state.ctx.viewport_id())
            .is_some_and(|vp| vp.repaint_delay == core::time::Duration::ZERO);
        *gate.pass_ever_ran = true;
    } else if !raw.events.is_empty() {
        // 面板跳过帧：事件积压（簿记已更新、读取器已排空，只欠"喂给 egui"）
        bridge.state.stash_events(std::mem::take(&mut raw.events));
        *gate.skipped_since_pass = true;
    }
    if gizmo_due {
        // gizmo 纯显示零交互：空事件子 pass（事件只消费一次，归面板 pass）
        state.ctx.set_pixels_per_point(ppp);
        state.ctx.begin_pass(egui::RawInput {
            screen_rect: raw_rect,
            time: raw_time,
            focused: raw_focused,
            ..Default::default()
        });
        // 坑：egui 按"上一个 pass 画过谁"记窗口可见性，本子 pass 不画窗口，不补记则
        // 下个面板 pass 视窗口为刚出现、重放淡入（~0.08s 低 alpha 进缓存）——表现为
        // "相机移动中面板只剩阴影"。move_to_top 补记；同 Order 内稳定排序，层序不变。
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
        frame.gizmo.generation += 1;
        *gate.pass_ever_ran = true;
    }
    *gate.last_size = size;
    frame.pixels_per_point = ppp;
    frame.screen_points = screen_points;
    // pass 自计时：两层合计；跳过帧为 0（监控读数如实反映节拍）
    ui.timing.last_ms = if *gate.pass_ever_ran && (panel_due || gizmo_due) {
        pass_clock.elapsed().as_secs_f32() * 1000.0
    } else {
        0.0
    };
    if !frame.panel.shapes.is_empty() || !frame.gizmo.shapes.is_empty() {
        gate.first_shapes_logged.log(|| {
            info!(
                "egui pass 统计：shapes 面板 {} + gizmo {}，ppp {ppp}，屏 {:.0}×{:.0}pt",
                frame.panel.shapes.len(),
                frame.gizmo.shapes.len(),
                frame.screen_points.x,
                frame.screen_points.y,
            );
        });
    }
}

/// 坑：egui 默认插件的 on_begin_pass/on_end_pass 只在 `run_ui` 闭包 API 里驱动，
/// begin_pass/end_pass 裸集成须自己补——否则 LabelSelectionState 的 is_dragging
/// 首次拖选后永不清零，选区随无按键悬停逐帧重投影。空 root Ui 仅作钩子宿主。
/// 其余默认插件在本宿主无负载（无回调注册/DnD/debug text），用到时照此补。
fn plugin_hooks_begin(state: &EguiState) -> egui::Ui {
    let mut plugin_host = egui::Ui::new(
        state.ctx.clone(),
        egui::Id::new((state.ctx.viewport_id(), "__top_ui")),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(state.ctx.viewport_rect()),
    );
    with_label_selection(state, |plugin| egui::Plugin::on_begin_pass(plugin, &mut plugin_host));
    plugin_host
}

/// 配对 [`plugin_hooks_begin`]：补跑 on_end_pass 后 `end_pass`。
fn plugin_hooks_end(state: &EguiState, mut plugin_host: egui::Ui) -> egui::FullOutput {
    with_label_selection(state, |plugin| egui::Plugin::on_end_pass(plugin, &mut plugin_host));
    state.ctx.end_pass()
}

/// 对 LabelSelectionState 插件跑一个钩子（插件缺席则跳过）。
fn with_label_selection(
    state: &EguiState,
    hook: impl FnOnce(&mut egui::text_selection::LabelSelectionState),
) {
    if let Some(label_selection) = state
        .ctx
        .plugin_opt::<egui::text_selection::LabelSelectionState>()
    {
        hook(&mut label_selection.lock());
    }
}
