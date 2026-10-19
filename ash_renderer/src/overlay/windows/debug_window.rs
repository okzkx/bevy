//! 调试窗口（业务面板）：overlay 的业务半边——"画什么"住这里，"怎么跑"住
//! [`crate::overlay::framework::ui`]。本文件只管帧统计窗口内容；加新调试窗口 =
//! 独立 rs 文件 struct（照 [`super::entity_tree_window`] 样式）+
//! [`super::debug_hub_window`] 加开关字段 + pass 里加显示门。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::scene::mechanism::scene_gen::SceneChunk;
use crate::vulkan::{
    BindlessTables, CullDiagnostics, CullSettings, FrameSample, ImageCache, IndirectStats,
    MeshPool, PerfLedger, Uploader, FRAME_MODE_LAMBERT, FRAME_MODE_NORMAL, FRAME_MODE_UNLIT,
};
use crate::scene::mechanism::WorkloadSettings;

use crate::overlay::framework::paint::AtlasGpu;

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
    pub(in crate::overlay) fn tick(&mut self, delta_secs: f32) {
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

/// chunk 盘点查询（5.4 计量行数据源）：根实体带 SceneChunk，成员数 =
/// 直接子实体数。生成期归属原样报出（不按当前位置重算）。
pub(in crate::overlay) type ChunkRows<'w, 's> =
    Query<'w, 's, (&'static SceneChunk, Option<&'static Children>)>;

/// 调试窗口的一次构建输入：框架 pass 每帧组好、字段名逐项交给面板。
pub(in crate::overlay) struct DebugWindow<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
    /// 显隐开关：总控 checkbox 与窗口 [×] 写同一字段（egui `Window::open` 借用）。
    pub(in crate::overlay) open: &'a mut bool,
    /// 帧率显示快照（pass 侧 FpsMeter 每 interval 刷新）。
    pub(in crate::overlay) fps: FpsSnapshot,
    pub(in crate::overlay) window: &'a Window,
    /// 本帧 pixels_per_point（窗口信息行展示）。
    pub(in crate::overlay) ppp: f32,
    /// 单选钮直接写它（借用自 pass 侧 ResMut，就地落资源）。
    pub(in crate::overlay) mode: &'a mut RenderMode,
    pub(in crate::overlay) stats: &'a RendererStats<'a>,
    /// 剔除开关（checkbox 直接写，draw_frame 读；BRP 与此同源）。
    pub(in crate::overlay) cull_settings: &'a mut CullSettings,
    /// 剔除诊断读数（draw_frame 每帧刷新；只进计量，不驱动绘制）。
    pub(in crate::overlay) cull_diag: &'a CullDiagnostics,
    /// indirect 闭环读数（draw_frame 每帧刷新；只进计量，不驱动绘制）。
    pub(in crate::overlay) indirect_stats: &'a IndirectStats,
    /// 帧样本账（5.5：分位数在构建时现算，环形窗口）。
    pub(in crate::overlay) perf: &'a PerfLedger,
    /// 变更形态开关（5.5：注入读数，BRP workload 与此同源）。
    pub(in crate::overlay) workload: &'a WorkloadSettings,
    /// 贴图缓存（5.5：显存字节读数）。
    pub(in crate::overlay) image_cache: &'a ImageCache,
    /// 网格池（5.5：顶点/索引 used/capacity 字节读数）。
    pub(in crate::overlay) mesh_pool: &'a MeshPool,
    /// chunk 盘点（5.4：块数与成员数计量行；明细走 BRP chunk_census）。
    pub(in crate::overlay) chunks: &'a ChunkRows<'a, 'a>,
}

impl DebugWindow<'_> {
    /// 面板本体：fps/窗口信息 + 着色模式单选 + 剔除开关与读数 + 渲染器内部统计
    ///（vulkan 侧资源只读）。
    pub(in crate::overlay) fn show(self) {
        let Self { ctx, open, fps, window, ppp, mode, stats, cull_settings, cull_diag, indirect_stats, perf, workload, image_cache, mesh_pool, chunks } = self;
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
                // 5.2 视锥剔除：开关切 compute dispatch / host 全可见填充
                //（数据层 A/B——两条路径都不进绘制，画面不变）；读数为帧槽
                // 直读（陈旧 ≤2 帧），只进计量（判定线 2 边界）。
                ui.checkbox(&mut cull_settings.enabled, "视锥剔除（compute）");
                ui.monospace(format!(
                    "标记 可见 {}/{} 行（bound {}，帧槽 {} 读数，诊断不驱动绘制）",
                    cull_diag.visible_count,
                    cull_diag.rows_compared,
                    cull_diag.bound,
                    cull_diag.marks_frame,
                ));
                // 5.3 indirect 闭环：绘制调用数恒 1（判定线 1 读数）、命令条数
                // 与 instanceCount=0 条数（诊断标注，不驱动绘制）
                ui.monospace(format!(
                    "indirect 提交 {} 次 · 命令 {} 条（零实例 {}，在场走访 {} 行暂缓 {}）——诊断不驱动绘制",
                    indirect_stats.draw_calls_recorded,
                    indirect_stats.command_count,
                    indirect_stats.zero_instance_count,
                    indirect_stats.walk_rows,
                    indirect_stats.pending_rows,
                ));
                // 5.5 分档压测：帧时间分位数 + 注入开关 + 显存字节（环形窗口
                // 现算；GPU 区间 = 本槽两帧前的提交，混合口径）
                // 三个字段的分位数各算各的（闭包异型不成数组，逐行展开）
                let show_pct = |ui: &mut egui::Ui, label: &str, field: &dyn Fn(&FrameSample) -> f32| {
                    let pct = |p: f32| perf.percentile(p, field).unwrap_or(0.0);
                    ui.monospace(format!(
                        "{label} p50 {:.2} · p95 {:.2} · p99 {:.2} ms（{n} 样本）",
                        pct(0.50),
                        pct(0.95),
                        pct(0.99),
                        n = perf.len(),
                    ));
                };
                show_pct(ui, "CPU", &|s| s.cpu_total_ms);
                show_pct(ui, "GPU 全帧", &|s| s.gpu_total_ms);
                show_pct(ui, "GPU 图形", &|s| s.gpu_graphics_ms);
                ui.monospace(format!(
                    "变更注入 {}（rows {}）——只进计量，诊断不驱动绘制",
                    if workload.enabled { "开" } else { "关" },
                    workload.rows,
                ));
                let ((v_used, v_cap), (i_used, i_cap)) = mesh_pool.usage();
                ui.monospace(format!(
                    "显存 池顶点 {:.1}/{:.1}MB · 索引 {:.1}/{:.1}MB · 贴图 {} 张 {:.1}MB（停尸房 {}）",
                    v_used as f64 / (1024.0 * 1024.0),
                    v_cap as f64 / (1024.0 * 1024.0),
                    i_used as f64 / (1024.0 * 1024.0),
                    i_cap as f64 / (1024.0 * 1024.0),
                    image_cache.resident_count(),
                    image_cache.resident_bytes() as f64 / (1024.0 * 1024.0),
                    image_cache.graveyard_len(),
                ));
                // 5.4 常驻大场景：chunk 组织读数（块数/成员总数；判定线 1 的
                // 面板侧读数，块明细与整块增删走 BRP census/despawn）
                let (chunk_count, chunk_members) = chunks
                    .iter()
                    .fold((0usize, 0usize), |(n, m), (_, c)| {
                        (n + 1, m + c.map_or(0, |c| c.len()))
                    });
                ui.monospace(format!(
                    "chunk {chunk_count} 块 · 成员 {chunk_members}（生成期归属，明细走 BRP chunk_census）"
                ));
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
