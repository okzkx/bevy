//! egui 状态与 pass：Update 里跑一遍 egui（纯 CPU），产 [`EguiFrame`] 交绘制半边。
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
//!
//! `RenderMode` 是场景着色模式（原 driver 帧 Local 的 env 解析，3.7 资源化）：
//! 本模块的单选钮写，`draw_frame` 读——依赖方向 driver→overlay 正向。

use std::sync::Arc;

use bevy::{
    input::{
        keyboard::{KeyCode, KeyboardInput},
        mouse::{MouseButtonInput, MouseWheel},
        ButtonInput,
    },
    prelude::*,
    window::{CursorLeft, CursorMoved, PrimaryWindow, Window, WindowFocused},
};

use crate::vulkan::{FRAME_MODE_LAMBERT, FRAME_MODE_NORMAL, FRAME_MODE_UNLIT};

use super::input::{egui_raw_input, EguiInput};

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

/// 场景着色模式（UBO `mode` 的宿主侧形态）。UI 单选钮写、`draw_frame` 读；
/// 初值来自 env `ASH_RENDER_MODE`（拼写错误 warn 后按 lambert 继续）。
#[derive(Clone, Copy, PartialEq, Eq, Resource)]
pub enum RenderMode {
    /// 数据驱动 Lambert（默认）：UBO 方向光 + 环境，bevy 物理链同构。
    Lambert,
    /// albedo 直出：base color/UV/颜色空间对照（官方侧同款置 unlit）。
    Unlit,
    /// 世界法线可视化：法线方向验收仪器（非均匀缩放案例的判定仪器）。
    Normal,
}

impl RenderMode {
    fn from_env() -> Self {
        match std::env::var("ASH_RENDER_MODE").ok().as_deref() {
            None | Some("") | Some("lambert") => Self::Lambert,
            Some("unlit") => Self::Unlit,
            Some("normal") => Self::Normal,
            Some(other) => {
                // 拼写错误是配置问题不是运行故障：warn 后按默认继续，不中断帧循环
                bevy::log::warn!(
                    "ASH_RENDER_MODE={other:?} 无法识别（可选 lambert/unlit/normal），按 lambert 继续"
                );
                Self::Lambert
            }
        }
    }

    /// UBO `mode` 值（与 `debug_draw.wgsl` 的 `MODE_*` 同值，`FRAME_MODE_*`）。
    pub(crate) fn as_u32(self) -> u32 {
        match self {
            Self::Lambert => FRAME_MODE_LAMBERT,
            Self::Unlit => FRAME_MODE_UNLIT,
            Self::Normal => FRAME_MODE_NORMAL,
        }
    }
}

/// Startup：建 egui Context + 装字体 + 插状态资源。
fn init_egui(mut commands: Commands) {
    let state = EguiState {
        ctx: egui::Context::default(),
    };
    state.ctx.set_fonts(load_fonts());
    commands.insert_resource(state);
    commands.insert_resource(EguiFrame::default());
    commands.insert_resource(RenderMode::from_env());
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

/// Update：组 RawInput → begin_pass → 调试窗口 → end_pass → 存 [`EguiFrame`]。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：六个事件读取器 + 窗口/时间/按键/两状态资源，逐项声明是框架惯例"
)]
fn run_egui_pass(
    state: ResMut<EguiState>,
    mut frame: ResMut<EguiFrame>,
    mut input: ResMut<EguiInput>,
    window: Query<&Window, With<PrimaryWindow>>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<RenderMode>,
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
    debug_window(&state.ctx, &time, window, ppp, &mut mode);
    let mut output = state.ctx.end_pass();
    // 纹理增量（字体图集 dirty-rect）在 pass 出口就地消费：`TexturesDelta` 的
    // Drop 审查要求增量被处理（否则 panic），epaint 文档的显式弃置出口是 `clear`。
    // 3.7.2 起换成真实消费——折进图集 CPU 镜像（Update 侧，最小化帧也不丢数据）
    // 后由绘制半边整传新槽；增量本身不跨系统存放。
    let (set_count, free_count) = {
        let delta = &output.textures_delta;
        (delta.set.len(), delta.free.len())
    };
    output.textures_delta.clear();
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

/// 调试窗口本体。渲染器内部统计（驻留/槽位/票据）接在 3.7.3（资源就绪后）。
fn debug_window(
    ctx: &egui::Context,
    time: &Time,
    window: &Window,
    ppp: f32,
    mode: &mut RenderMode,
) {
    egui::Window::new("ash 调试")
        .default_pos(egui::pos2(12.0, 12.0))
        .show(ctx, |ui| {
            let fps = 1.0 / time.delta_secs().max(1e-6);
            ui.monospace(format!("fps {:.0}  ({:.1} ms)", fps, time.delta_secs() * 1000.0));
            ui.monospace(format!(
                "窗口 {:.0}×{:.0}pt（物理 {:.0}×{:.0}）ppp {ppp:.2}",
                window.width(),
                window.height(),
                window.physical_width(),
                window.physical_height(),
            ));
            ui.separator();
            ui.label("着色模式");
            ui.horizontal(|ui| {
                ui.selectable_value(mode, RenderMode::Lambert, "lambert");
                ui.selectable_value(mode, RenderMode::Unlit, "unlit");
                ui.selectable_value(mode, RenderMode::Normal, "normal");
            });
        });
}
