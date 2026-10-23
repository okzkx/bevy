//! 帧统计页：fps/窗口信息、帧时间分位数、显存与常驻表等渲染器内部读数（全只读）。
//! 帧率快照与分位数缓存也住这里——pass 每帧推进，各页只读展示。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::overlay::framework::paint::AtlasGpu;
use crate::render::{BindlessTables, ImageCache, MeshPool, PerfLedger, Uploader};
use crate::scene::mechanism::scene_gen::SceneChunk;
use crate::scene::mechanism::WorkloadSettings;

/// vulkan 侧统计三项的只读束（帧统计页取数）。打成 SystemParam 是因为函数系统的
/// 参数上限 16 个（FrameInput/UiDrawData 同款式样）。
#[derive(SystemParam)]
pub(in crate::overlay) struct RendererStats<'w> {
    tables: Res<'w, BindlessTables>,
    uploader: Res<'w, Uploader>,
    gpu: Res<'w, AtlasGpu>,
}

/// 逐帧 delta 抖动大，直接显示 fps 数字每帧都在跳——累计一个更新间隔的帧数
/// 与时长，到点出一次平均快照。
#[derive(Resource)]
pub(in crate::overlay) struct FpsMeter {
    acc_secs: f32,
    acc_frames: u32,
    /// 当前显示快照（每 interval 刷新，间隔内保持不变）。
    pub(in crate::overlay) snapshot: FpsSnapshot,
}

/// 一次帧率显示快照：fps 与平均帧耗时（毫秒）。
#[derive(Clone, Copy)]
pub(in crate::overlay) struct FpsSnapshot {
    pub(in crate::overlay) fps: f32,
    pub(in crate::overlay) ms: f32,
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
    /// 返回本次是否刚刷新了快照（7.5 监控采样复用同一节奏：新快照才入历史）。
    pub(in crate::overlay) fn tick(&mut self, delta_secs: f32) -> bool {
        self.acc_secs += delta_secs;
        self.acc_frames += 1;
        if self.acc_secs >= Self::UPDATE_INTERVAL {
            let avg = self.acc_secs / self.acc_frames.max(1) as f32;
            self.snapshot = FpsSnapshot { fps: 1.0 / avg.max(1e-6), ms: avg * 1000.0 };
            self.acc_secs = 0.0;
            self.acc_frames = 0;
            true
        } else {
            false
        }
    }
}

/// 分位数面板缓存（7.5）：跟 fps 快照同节奏（0.5s）重算三分位数 × 三字段，
/// 每帧只读展示——逐帧九次全量 sort 已随 [`PerfLedger::percentiles`] 收敛为
/// 每字段一趟，且文本 0.5s 才变一次（egui galley 缓存可命中，不再逐帧重塑）。
#[derive(Resource, Default)]
pub(in crate::overlay) struct PerfPanelCache {
    /// (p50, p95, p99, 样本数) 逐字段。
    pub(in crate::overlay) cpu: (f32, f32, f32, usize),
    pub(in crate::overlay) gpu_total: (f32, f32, f32, usize),
    pub(in crate::overlay) gpu_lighting: (f32, f32, f32, usize),
}

const PANEL_PS: [f32; 3] = [0.50, 0.95, 0.99];

impl PerfPanelCache {
    /// fps 快照刷新时调用（0.5s 一次），整块重算。
    pub(in crate::overlay) fn refresh(&mut self, perf: &PerfLedger) {
        let take = |v: Vec<Option<f32>>| {
            let n = v.iter().filter(|x| x.is_some()).count();
            (
                v.first().copied().flatten().unwrap_or(0.0),
                v.get(1).copied().flatten().unwrap_or(0.0),
                v.get(2).copied().flatten().unwrap_or(0.0),
                n,
            )
        };
        self.cpu = take(perf.percentiles(&PANEL_PS, |s| s.cpu_total_ms));
        self.gpu_total = take(perf.percentiles(&PANEL_PS, |s| s.gpu_total_ms));
        self.gpu_lighting = take(perf.percentiles(&PANEL_PS, |s| s.gpu_lighting_ms));
    }
}

/// chunk 盘点查询（5.4 计量行数据源）：根实体带 SceneChunk，成员数 =
/// 直接子实体数。生成期归属原样报出（不按当前位置重算）。
pub(in crate::overlay) type ChunkRows<'w, 's> =
    Query<'w, 's, (&'static SceneChunk, Option<&'static Children>)>;


/// 帧统计页的一次构建输入：框架 pass 每帧组好、字段名逐项交给页面。
pub(in crate::overlay) struct FrameStatsPage<'a> {
    /// 帧率显示快照（pass 侧 FpsMeter 每 interval 刷新）。
    pub(in crate::overlay) fps: FpsSnapshot,
    pub(in crate::overlay) window: &'a Window,
    /// 本帧 pixels_per_point（窗口信息行展示）。
    pub(in crate::overlay) ppp: f32,
    pub(in crate::overlay) stats: &'a RendererStats<'a>,
    /// 分位数面板缓存（pass 侧 0.5s 重算，本页只读）。
    pub(in crate::overlay) pct: &'a PerfPanelCache,
    /// 变更注入开关（只读展示，BRP workload 写）。
    pub(in crate::overlay) workload: &'a WorkloadSettings,
    pub(in crate::overlay) image_cache: &'a ImageCache,
    pub(in crate::overlay) mesh_pool: &'a MeshPool,
    /// chunk 盘点（块数与成员数；明细走 BRP chunk_census）。
    pub(in crate::overlay) chunks: &'a ChunkRows<'a, 'a>,
}

impl FrameStatsPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { fps, window, ppp, stats, pct, workload, image_cache, mesh_pool, chunks } = self;
        ui.monospace(format!("fps {:.0}  ({:.1} ms)", fps.fps, fps.ms));
        ui.monospace(format!(
            "窗口 {:.0}×{:.0}pt（物理 {:.0}×{:.0}）ppp {ppp:.2}",
            window.width(),
            window.height(),
            window.physical_width(),
            window.physical_height(),
        ));
        ui.separator();
        // 帧时间分位数：GPU 区间 = 本槽两帧前的提交，与 CPU 不是同一帧
        ui.strong("帧时间分位数");
        let show_pct = |ui: &mut egui::Ui, label: &str, row: (f32, f32, f32, usize)| {
            ui.monospace(format!(
                "{label} p50 {:.2} · p95 {:.2} · p99 {:.2} ms（{n} 样本）",
                row.0, row.1, row.2, n = row.3,
            ));
        };
        show_pct(ui, "CPU", pct.cpu);
        show_pct(ui, "GPU 全帧", pct.gpu_total);
        show_pct(ui, "GPU 图形", pct.gpu_lighting);
        ui.monospace(format!(
            "变更注入 {}（rows {}）",
            if workload.enabled { "开" } else { "关" },
            workload.rows,
        ));
        ui.separator();
        ui.strong("资源");
        let ((v_used, v_cap), (i_used, i_cap)) = mesh_pool.usage();
        const MB: f64 = 1024.0 * 1024.0;
        ui.monospace(format!(
            "网格池 顶点 {:.1}/{:.1}MB · 索引 {:.1}/{:.1}MB",
            v_used as f64 / MB,
            v_cap as f64 / MB,
            i_used as f64 / MB,
            i_cap as f64 / MB,
        ));
        ui.monospace(format!(
            "贴图 {} 张 {:.1}MB（停尸房 {}）",
            image_cache.resident_count(),
            image_cache.resident_bytes() as f64 / MB,
            image_cache.graveyard_len(),
        ));
        let (chunk_count, chunk_members) = chunks
            .iter()
            .fold((0usize, 0usize), |(n, m), (_, c)| (n + 1, m + c.map_or(0, |c| c.len())));
        ui.monospace(format!("chunk {chunk_count} 块 · 成员 {chunk_members}（生成期归属）"));
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
    }
}
