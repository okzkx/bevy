//! egui 框架半边：Startup 建状态，Update 里跑一遍 egui pass（纯 CPU），产
//! [`EguiFrame`] 交绘制半边。
//!
//! overlay 按内容分两半：本文件是框架半边——"怎么跑"（Context/字体、pass、
//! 纹理增量折叠、帧产出）；窗口内容"画什么"是业务，住 [`super::debug_window`]、
//! [`super::debug_hub_window`] 与 [`super::entity_tree_window`]（本文件的 pass
//! 只负责按总控开关在 begin/end 之间调它们）。
//!
//! pass 三件（egui 0.36，源码钉死见施工计划 §2.1）：`begin_pass(RawInput)` →
//! 建窗口 UI → `end_pass() -> FullOutput`。`FullOutput.shapes` 是**未镶嵌**的
//! 原始形状——镶嵌（`ctx.tessellate`）归绘制半边在 `draw_frame` 内做（写顶点环
//! 须过 `wait_for_slot` 的 fence，见施工计划 §3.2）。
//!
//! 字体：egui `default_fonts`（拉丁字形）+ Windows 系统微软雅黑
//! （`C:\Windows\Fonts\msyh.ttc`，face index 0）插进 Proportional 回退位——拉丁
//! 仍走自带 Ubuntu，CJK 落到雅黑。读不到 warn 后继续（Tier①：调试 UI 缺 CJK
//! 显示为豆腐块，不影响帧循环）。

use std::sync::Arc;

use bevy::{
    ecs::system::SystemParam,
    input::{
        keyboard::{KeyCode, KeyboardInput},
        mouse::{MouseButtonInput, MouseWheel},
        ButtonInput,
    },
    prelude::*,
    window::{CursorLeft, CursorMoved, PrimaryWindow, Window, WindowFocused},
};

use super::debug_hub_window::{DebugHubWindow, DebugWindowsOpen};
use super::debug_window::{DebugWindow, FpsMeter, RenderMode, RendererStats};
use super::entity_tree_window::{EntityTreeData, EntityTreeWindow, SelectedEntity};
use super::input::{egui_raw_input, EguiInput};
use super::paint::{AtlasGpu, AtlasMirror};

use ash_macros::system;

/// 调试 UI 插件：Startup 建状态，Update 跑 egui pass。绘制半边（图集/顶点/管线）
/// 在 `driver`/`vulkan` 侧，消费本插件产的 [`EguiFrame`]。
pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, init_egui)
            .init_resource::<EguiInput>()
            .add_systems(Update, run_egui_pass);
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
    /// 未镶嵌的原始形状（左上原点 points 域）。
    pub shapes: Vec<egui::epaint::ClippedShape>,
    /// 本帧 pixels_per_point（= 窗口 scale_factor）。
    pub pixels_per_point: f32,
    /// 屏幕 points 尺寸（scissor 换算的分母域）。
    pub screen_points: Vec2,
}

/// Startup：建 egui Context + 装字体 + 插状态资源。
#[system]
fn init_egui(mut commands: Commands) {
    let state = EguiState {
        ctx: egui::Context::default(),
    };
    state.ctx.set_fonts(load_fonts());
    commands.insert_resource(state);
    commands.insert_resource(EguiFrame::default());
    commands.insert_resource(RenderMode::from_env());
    // 各调试窗口显隐总控（3.10）：总控面板写、pass 显示门读，默认全开
    commands.insert_resource(DebugWindowsOpen::default());
    // 层级树点选（3.10）：3.11 编辑面板的目标来源
    commands.insert_resource(SelectedEntity::default());
    // 帧率显示平滑（3.10 收官后追记）：0.5s 出一次平均快照
    commands.insert_resource(FpsMeter::default());
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
/// 的束打法），3.10 新增件全走这里，不再撑大 pass 签名。
#[derive(SystemParam)]
struct DebugUiParams<'w, 's> {
    stats: RendererStats<'w>,
    mode: ResMut<'w, RenderMode>,
    windows_open: ResMut<'w, DebugWindowsOpen>,
    selected: ResMut<'w, SelectedEntity>,
    fps: ResMut<'w, FpsMeter>,
    tree: EntityTreeData<'w, 's>,
}

/// Update：组 RawInput → begin_pass → 总控 + 各调试窗口（按显隐开关）→
/// end_pass → 存 [`EguiFrame`]。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：六个事件读取器 + 窗口/时间/按键 + egui 状态帧与图集镜像资源 + 调试窗口面板束，逐项声明是框架惯例"
)]
#[system]
fn run_egui_pass(
    state: ResMut<EguiState>,
    mut frame: ResMut<EguiFrame>,
    mut input: ResMut<EguiInput>,
    mut mirror: ResMut<AtlasMirror>,
    mut ui: DebugUiParams,
    window: Query<&Window, With<PrimaryWindow>>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut keyboard: MessageReader<KeyboardInput>,
    mut mouse_button: MessageReader<MouseButtonInput>,
    mut wheel: MessageReader<MouseWheel>,
    mut cursor: MessageReader<CursorMoved>,
    mut cursor_left: MessageReader<CursorLeft>,
    mut window_focused: MessageReader<WindowFocused>,
    mut first_shapes_logged: Local<bool>,
) {
    let Some(window) = window.iter().next() else {
        return; // 主窗未建（Startup 前的空帧）：egui 无屏幕域，整帧让路
    };
    let ppp = window.scale_factor();
    let screen_points = Vec2::new(window.width(), window.height());
    let raw =
        egui_raw_input(
            window, &time, &keys, &mut input, &mut keyboard, &mut mouse_button, &mut wheel,
            &mut cursor, &mut cursor_left, &mut window_focused,
        );
    state.ctx.set_pixels_per_point(ppp);
    state.ctx.begin_pass(raw);
    // 帧率快照先喂再显示：0.5s 一刷，间隔内读数稳定
    ui.fps.tick(time.delta_secs());
    // 总控先行（自身不可关），其余窗口按开关显隐；[×] 与 checkbox 写同一字段
    DebugHubWindow { ctx: &state.ctx, open: &mut ui.windows_open }.show();
    if ui.windows_open.stats {
        DebugWindow {
            ctx: &state.ctx,
            open: &mut ui.windows_open.stats,
            fps: ui.fps.snapshot,
            window,
            ppp,
            mode: &mut ui.mode,
            stats: &ui.stats,
        }
        .show();
    }
    if ui.windows_open.entity_tree {
        EntityTreeWindow {
            ctx: &state.ctx,
            open: &mut ui.windows_open.entity_tree,
            selected: &mut ui.selected,
            data: &ui.tree,
        }
        .show();
    }
    let mut output = state.ctx.end_pass();
    // 纹理增量（字体图集 dirty-rect）在 pass 出口就地消费：折进图集 CPU 镜像
    //（Update 侧——最小化帧 Update 照跑而 draw_frame 让路，折入不丢数据），折完
    // clear 满足 TexturesDelta 的 Drop 审查。镜像整传/新槽发布由绘制半边按
    // 代数差触发（paint_overlay）。
    let (set_count, free_count) = {
        let delta = &output.textures_delta;
        (delta.set.len(), delta.free.len())
    };
    mirror.fold(&mut output.textures_delta);
    *frame = EguiFrame {
        shapes: output.shapes,
        pixels_per_point: ppp,
        screen_points,
    };
    if !*first_shapes_logged && !frame.shapes.is_empty() {
        *first_shapes_logged = true;
        info!(
            "egui pass 收账：shapes {}，ppp {ppp}，屏 {:.0}×{:.0}pt，\
             纹理增量 set {set_count}/free {free_count}（图集变化才非空）",
            frame.shapes.len(),
            frame.screen_points.x,
            frame.screen_points.y,
        );
    }
}
