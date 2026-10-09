//! 调试窗口（业务面板）：overlay 的业务半边——"画什么"住这里，"怎么跑"住
//! [`super::ui`]。本文件只管帧统计窗口内容；加新调试窗口 = 独立 rs 文件 struct
//!（照 [`super::entity_tree_window`] 样式）+ [`super::debug_hub_window`] 加开关
//! 字段 + pass 里加显示门。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::vulkan::{
    BindlessTables, Uploader, FRAME_MODE_LAMBERT, FRAME_MODE_NORMAL, FRAME_MODE_UNLIT,
};

use super::paint::AtlasGpu;

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
    pub(crate) fn from_env() -> Self {
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

/// vulkan 侧统计三项的只读束（调试窗口取数）。打成 SystemParam 是因为函数系统的
/// 参数上限 16 个（FrameInput/UiDrawData 同款式样）。
#[derive(SystemParam)]
pub(super) struct RendererStats<'w> {
    tables: Res<'w, BindlessTables>,
    uploader: Res<'w, Uploader>,
    gpu: Res<'w, AtlasGpu>,
}

/// 帧率显示平滑：累计一个更新间隔的帧数与时长，到点出一次平均快照。
/// 逐帧 delta 抖动大，直接显示 fps 数字每帧都在跳（3.10 收官后用户反馈）。
#[derive(Resource)]
pub(super) struct FpsMeter {
    acc_secs: f32,
    acc_frames: u32,
    /// 当前显示快照（每 interval 刷新，间隔内保持不变）。
    pub(super) snapshot: FpsSnapshot,
}

/// 一次帧率显示快照：fps 与平均帧耗时（毫秒）。
#[derive(Clone, Copy)]
pub(super) struct FpsSnapshot {
    pub(super) fps: f32,
    pub(super) ms: f32,
}

impl Default for FpsMeter {
    fn default() -> Self {
        Self { acc_secs: 0.0, acc_frames: 0, snapshot: FpsSnapshot { fps: 0.0, ms: 0.0 } }
    }
}

impl FpsMeter {
    /// 显示值刷新间隔：0.5s = 每秒两跳，稳定且不算迟钝。
    const UPDATE_INTERVAL: f32 = 0.5;

    /// pass 每帧喂一次本帧 delta；累计满一个 interval 出一次新快照。
    pub(super) fn tick(&mut self, delta_secs: f32) {
        self.acc_secs += delta_secs;
        self.acc_frames += 1;
        if self.acc_secs >= Self::UPDATE_INTERVAL {
            let avg = self.acc_secs / self.acc_frames.max(1) as f32;
            self.snapshot = FpsSnapshot { fps: 1.0 / avg.max(1e-6), ms: avg * 1000.0 };
            self.acc_secs = 0.0;
            self.acc_frames = 0;
        }
    }
}

/// 调试窗口的一次构建输入：框架 pass 每帧组好、字段名逐项交给面板。
pub(super) struct DebugWindow<'a> {
    pub(super) ctx: &'a egui::Context,
    /// 显隐开关：总控 checkbox 与窗口 [×] 写同一字段（egui `Window::open` 借用）。
    pub(super) open: &'a mut bool,
    /// 帧率显示快照（pass 侧 FpsMeter 每 interval 刷新）。
    pub(super) fps: FpsSnapshot,
    pub(super) window: &'a Window,
    /// 本帧 pixels_per_point（窗口信息行展示）。
    pub(super) ppp: f32,
    /// 单选钮直接写它（借用自 pass 侧 ResMut，就地落资源）。
    pub(super) mode: &'a mut RenderMode,
    pub(super) stats: &'a RendererStats<'a>,
}

impl DebugWindow<'_> {
    /// 面板本体：fps/窗口信息 + 着色模式单选 + 渲染器内部统计（vulkan 侧资源只读）。
    pub(super) fn show(self) {
        let Self { ctx, open, fps, window, ppp, mode, stats } = self;
        egui::Window::new("ash 调试")
            .open(open)
            .default_pos(egui::pos2(12.0, 12.0))
            .show(ctx, |ui| {
                ui.monospace(format!("fps {:.0}  ({:.1} ms)", fps.fps, fps.ms));
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
                ui.separator();
                ui.monospace(format!(
                    "常驻表 纹理 {}/{} · 采样器 {}",
                    stats.tables.used_texture_slots(),
                    stats.tables.capacity(),
                    stats.tables.used_sampler_slots(),
                ));
                ui.monospace(format!(
                    "图集 第 {} 代 · graveyard {} 张",
                    stats.gpu.generation(),
                    stats.gpu.graveyard_len(),
                ));
                ui.monospace(format!("票据 #{}", stats.uploader.last_issued_ticket()));
            });
    }
}
