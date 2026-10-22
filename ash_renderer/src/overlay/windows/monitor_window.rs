//! 持续监控窗口（业务面板）：FPS 历史/进程内存/系统内存/显存预算的时间序列
//! 读数——现有观察面只有 0.5s fps 快照与瞬时池读数，抓不到"随时间恶化"的
//! 形态（7.5 立案动机）。采样随 fps 快照节奏（0.5s）入
//! [`MonitorHistory`]，无论本窗口显隐都持续记账；BRP `monitor_stats`
//! 读同一本账，脚本无人值守采样与面板同源。
//!
//! 显存两口径并列：VK_EXT_memory_budget 的堆级 usage/budget 是**系统级**
//! 语义（含其他进程，扩展缺席则整行隐藏）；渲染器内部账（MeshPool/
//! ImageCache）才是本进程真实占用——两者差值趋势才可归因。

use bevy::prelude::*;

use super::debug_window::FpsSnapshot;
use crate::driver::host::FrameSyncSettings;
use crate::render::{Context, ImageCache, MeshPool};

/// 一个监控采样点（fps 快照刷新节奏入账，约 0.5s 一个）。
#[derive(Clone, Copy)]
pub(crate) struct MonitorSample {
    /// 起始以来秒数。
    pub(crate) elapsed: f32,
    pub(crate) fps: f32,
    pub(crate) ms: f32,
    /// 进程工作集字节（GetProcessMemoryInfo；API 失败为 None）。
    pub(crate) proc_working_set: Option<u64>,
    /// 系统物理内存 总量/可用 字节（GlobalMemoryStatusEx；失败为 None）。
    pub(crate) sys: Option<(u64, u64)>,
    /// 设备局部堆 usage/budget 字节（VK_EXT_memory_budget，系统级语义；
    /// 扩展缺席为 None）。
    pub(crate) vram: Option<(u64, u64)>,
}

/// 监控历史账：采样点环形累积（满容量从头挤掉最旧），窗口内统计与曲线
/// 都从 `samples` 现算——不做增量聚合，1 小时窗 7200 点的量级不值得。
#[derive(Resource, Default)]
pub(crate) struct MonitorHistory {
    /// 首个采样的时刻（elapsed 起点；Startup 后首刷才落，None = 未开账）。
    started: Option<std::time::Instant>,
    pub(crate) samples: Vec<MonitorSample>,
}

impl MonitorHistory {
    /// 容量：0.5s 节奏 × 7200 = 1 小时窗，超出挤掉最旧。
    const CAP: usize = 7200;

    /// 一次采样：fps 快照 + 进程/系统内存 + 显存堆读数。pass 侧在 fps 快照
    /// 刚刷新时调用（0.5s 节奏），窗口显隐不影响记账。
    pub(crate) fn sample(&mut self, fps: f32, ms: f32, ctx: Option<&Context>) {
        let elapsed = match self.started {
            Some(t) => t.elapsed().as_secs_f32(),
            None => {
                self.started = Some(std::time::Instant::now());
                0.0
            }
        };
        let vram = ctx.and_then(|ctx| ctx.memory_budget()).map(|heaps| {
            // 设备局部堆可能有多个（独立显卡 + 小显存堆）：usage/budget 求和
            heaps
                .iter()
                .filter(|h| h.device_local)
                .fold((0u64, 0u64), |(u, b), h| (u + h.usage, b + h.budget))
        });
        self.samples.push(MonitorSample {
            elapsed,
            fps,
            ms,
            proc_working_set: process_working_set(),
            sys: system_memory(),
            vram,
        });
        if self.samples.len() > Self::CAP {
            self.samples.drain(..self.samples.len() - Self::CAP);
        }
    }
}

/// 进程工作集字节。失败（句柄/权限异常）静默 None——监控读数缺一行
/// 不值一次报错。
#[cfg(windows)]
fn process_working_set() -> Option<u64> {
    use windows::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // # Safety:counters 为合法可写指针、cb 恰为结构体大小（API 契约）
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).ok()? };
    Some(counters.WorkingSetSize as u64)
}

#[cfg(not(windows))]
fn process_working_set() -> Option<u64> {
    None
}

/// 系统物理内存（总量, 可用）字节。
#[cfg(windows)]
fn system_memory() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // # Safety:status 为合法可写指针、dwLength 恰为结构体大小（API 契约）
    unsafe { GlobalMemoryStatusEx(&mut status).ok()? };
    Some((status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(not(windows))]
fn system_memory() -> Option<(u64, u64)> {
    None
}

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

/// 监控窗口的一次构建输入：框架 pass 每帧组好。
pub(crate) struct MonitorWindow<'a> {
    pub(crate) ctx: &'a egui::Context,
    pub(crate) open: &'a mut bool,
    /// 监控历史账（pass 持续采样，本窗口只读呈现）。
    pub(crate) history: &'a MonitorHistory,
    /// fps 快照（FpsMeter 当前值，账外当前读数）。
    pub(crate) fps: FpsSnapshot,
    /// 渲染器内部显存账（MeshPool/ImageCache，进程自身口径）。
    pub(crate) mesh_pool: &'a MeshPool,
    pub(crate) image_cache: &'a ImageCache,
    /// 帧率同步（7.6）：刷新率读数只读展示，checkbox 写开关
    ///（即时生效——apply_frame_sync 同帧读、下轮循环生效）。
    pub(crate) frame_sync: &'a mut FrameSyncSettings,
}

/// 曲线与统计的时间窗（秒）：面板聚焦近期形态，全史看导出样本。
const SPAN_SECS: f32 = 600.0;

impl MonitorWindow<'_> {
    /// 面板本体：fps 曲线 + 窗内统计 + 内存/显存读数行。
    pub(crate) fn show(self) {
        let Self { ctx, open, history, fps, mesh_pool, image_cache, frame_sync } = self;
        egui::Window::new("持续监控")
            .open(open)
            .default_pos(egui::pos2(200.0, 12.0))
            .default_width(360.0)
            .show(ctx, |ui| {
                let now = history.samples.last().map(|s| s.elapsed).unwrap_or(0.0);
                let span_start = (now - SPAN_SECS).max(0.0);
                let span: Vec<_> = history
                    .samples
                    .iter()
                    .filter(|s| s.elapsed >= span_start)
                    .collect();

                ui.monospace(format!("fps 当前 {:.0}（{:.1} ms）· 样本 {} · 已采 {:.0}s", fps.fps, fps.ms, history.samples.len(), now));

                // 帧率同步（7.6）：刷新率读数 + "同步 vsync"开关
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
                        Some(ws) => ui.monospace(format!("内存 进程工作集 {:.1} MB", ws as f64 / (1024.0 * 1024.0))),
                        None => ui.monospace("内存 进程工作集 不可用"),
                    };
                    match s.sys {
                        Some((total, avail)) => ui.monospace(format!(
                            "内存 系统 可用 {:.1}/{:.1} GB",
                            avail as f64 / (1024.0 * 1024.0 * 1024.0),
                            total as f64 / (1024.0 * 1024.0 * 1024.0),
                        )),
                        None => ui.monospace("内存 系统 不可用"),
                    };
                    match s.vram {
                        Some((usage, budget)) => ui.monospace(format!(
                            "显存 堆级 usage {:.0}/{:.0} MB（系统级语义，含其他进程）",
                            usage as f64 / (1024.0 * 1024.0),
                            budget as f64 / (1024.0 * 1024.0),
                        )),
                        None => ui.monospace("显存 堆级预算 不可用（VK_EXT_memory_budget 缺席）"),
                    };
                }
                let ((v_used, v_cap), (i_used, i_cap)) = mesh_pool.usage();
                ui.monospace(format!(
                    "显存 内部账 顶点 {:.1}/{:.1}MB · 索引 {:.1}/{:.1}MB · 贴图 {} 张 {:.1}MB（停尸房 {}）",
                    v_used as f64 / (1024.0 * 1024.0),
                    v_cap as f64 / (1024.0 * 1024.0),
                    i_used as f64 / (1024.0 * 1024.0),
                    i_cap as f64 / (1024.0 * 1024.0),
                    image_cache.resident_count(),
                    image_cache.resident_bytes() as f64 / (1024.0 * 1024.0),
                    image_cache.graveyard_len(),
                ));
            });
    }
}
