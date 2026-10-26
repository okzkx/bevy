//! 持续监控页：FPS 历史/进程内存/系统内存/显存预算的时间序列
//! 读数——0.5s fps 快照与瞬时池读数抓不到"随时间恶化"的形态，本页补时间
//! 维度。采样随 fps 快照节奏（0.5s）入
//! [`MonitorHistory`]，无论本页是否显示都持续记账；BRP `monitor_stats`
//! 读同一本账，脚本无人值守采样与面板同源。
//!
//! 显存两口径并列：VK_EXT_memory_budget 的堆级 usage/budget 是**系统级**
//! 语义（含其他进程，扩展缺席则整行隐藏）；渲染器内部账（MeshPool/
//! ImageCache）才是本进程真实占用——两者差值趋势才可归因。

use bevy::prelude::*;

use super::frame_stats::FpsSnapshot;
use super::{gib, mib};
use crate::render::{FrameSyncSettings, ImageCache, MeshPool, MonitorHistory};

/// 统计块（曲线窗口内现算）：min/p50/p95/max。
struct SpanStats {
    min: f32,
    p50: f32,
    p95: f32,
    max: f32,
    n: usize,
}

impl SpanStats {
    fn of(values: &mut [f32]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f32::total_cmp);
        let pick = |p: f32| values[((values.len() - 1) as f32 * p) as usize];
        Some(Self {
            min: values[0],
            p50: pick(0.50),
            p95: pick(0.95),
            max: values[values.len() - 1],
            n: values.len(),
        })
    }
}

/// 监控页的一次构建输入：框架 pass 每帧组好。
pub(crate) struct MonitorPage<'a> {
    /// 监控历史账（pass 持续采样，本页只读呈现）。
    pub(crate) history: &'a MonitorHistory,
    /// fps 快照（FpsMeter 当前值，账外当前读数）。
    pub(crate) fps: FpsSnapshot,
    /// 渲染器内部显存账（MeshPool/ImageCache，进程自身口径）。
    pub(crate) mesh_pool: &'a MeshPool,
    pub(crate) image_cache: &'a ImageCache,
    /// 帧率同步：刷新率读数只读展示，checkbox 写开关
    ///（即时生效——apply_frame_sync 同帧读、下轮循环生效）。
    pub(crate) frame_sync: &'a mut FrameSyncSettings,
}

/// 曲线与统计的时间窗（秒）：面板聚焦近期形态，全史看导出样本。
const SPAN_SECS: f32 = 600.0;

impl MonitorPage<'_> {
    /// 面板本体：fps 曲线 + 窗内统计 + 内存/显存读数行。
    pub(crate) fn show(self, ui: &mut egui::Ui) {
        let Self { history, fps, mesh_pool, image_cache, frame_sync } = self;
        let now = history.samples.last().map(|s| s.elapsed).unwrap_or(0.0);
        let span_start = (now - SPAN_SECS).max(0.0);
        let span: Vec<_> = history
            .samples
            .iter()
            .filter(|s| s.elapsed >= span_start)
            .collect();

        ui.monospace(format!("fps 当前 {:.0}（{:.1} ms）· 样本 {} · 已采 {:.0}s", fps.fps, fps.ms, history.samples.len(), now));

        // 帧率同步：刷新率读数 + "同步 vsync"开关
        //（语义 = 帧循环节奏对齐显示器刷新率，非 present mode 切换）
        match frame_sync.refresh_millihertz {
            Some(mhz) => ui.monospace(format!("屏幕刷新率 {:.0} Hz", f64::from(mhz) / 1000.0)),
            None => ui.monospace("屏幕刷新率 不可用"),
        };
        ui.checkbox(&mut frame_sync.enabled, "同步 vsync")
            .on_hover_text("帧循环节奏对齐显示器刷新率（winit wait），非 present mode 切换；关闭后聚焦时全速跑满");

        // fps 曲线（窗内）：y 轴 0 到窗内 max × 1.15 留头
        let (response, painter) =
            ui.allocate_painter(egui::vec2(ui.available_width(), 64.0), egui::Sense::hover());
        let rect = response.rect;
        if let Some(max_fps) = span.iter().map(|s| s.fps).fold(None::<f32>, |a, v| Some(a.map_or(v, |a| a.max(v)))) {
            let y_top = (max_fps * 1.15).max(1.0);
            let points: Vec<_> = span
                .iter()
                .map(|s| {
                    let t = (s.elapsed - span_start).max(0.0) / SPAN_SECS.max(1.0);
                    let y = 1.0 - (s.fps / y_top).clamp(0.0, 1.0);
                    egui::pos2(rect.left() + t * rect.width(), rect.top() + y * rect.height())
                })
                .collect();
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, egui::Color32::LIGHT_GREEN)));
        }
        if let Some(stats) = SpanStats::of(&mut span.iter().map(|s| s.fps).collect::<Vec<f32>>()) {
            ui.monospace(format!(
                "fps 窗内 min {:.0} · p50 {:.0} · p95 {:.0} · max {:.0}（{n} 样本 / {SPAN_SECS:.0}s）",
                stats.min, stats.p50, stats.p95, stats.max, n = stats.n,
            ));
        }
        if let Some(stats) = SpanStats::of(&mut span.iter().map(|s| s.ms).collect::<Vec<f32>>()) {
            ui.monospace(format!(
                "帧耗 窗内 p50 {:.2} · p95 {:.2} · max {:.2} ms",
                stats.p50, stats.p95, stats.max,
            ));
        }

        ui.separator();
        if let Some(s) = history.samples.last() {
            match s.proc_working_set {
                Some(ws) => ui.monospace(format!("内存 进程工作集 {:.1} MB", mib(ws))),
                None => ui.monospace("内存 进程工作集 不可用"),
            };
            match s.sys {
                Some((total, avail)) => ui.monospace(format!(
                    "内存 系统 可用 {:.1}/{:.1} GB",
                    gib(avail),
                    gib(total),
                )),
                None => ui.monospace("内存 系统 不可用"),
            };
            match s.vram {
                Some((usage, budget)) => ui.monospace(format!(
                    "显存 堆级 usage {:.0}/{:.0} MB（系统级语义，含其他进程）",
                    mib(usage),
                    mib(budget),
                )),
                None => ui.monospace("显存 堆级预算 不可用（VK_EXT_memory_budget 缺席）"),
            };
        }
        let ((v_used, v_cap), (i_used, i_cap)) = mesh_pool.usage();
        ui.monospace(format!(
            "显存 内部账 顶点 {:.1}/{:.1}MB · 索引 {:.1}/{:.1}MB · 贴图 {} 张 {:.1}MB（停尸房 {}）",
            mib(v_used),
            mib(v_cap),
            mib(i_used),
            mib(i_cap),
            image_cache.resident_count(),
            mib(image_cache.resident_bytes()),
            image_cache.graveyard_len(),
        ));
    }
}
