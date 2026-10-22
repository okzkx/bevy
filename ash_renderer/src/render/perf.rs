//! GPU timestamp 计时与帧样本账（5.5.2，6.1.2 起五位）：统一口径 §3"GPU 用
//! 对应工作区间计时"的落点——每帧槽一只 TIMESTAMP query pool（5 个查询位），
//! 帧内写 5 个 timestamp，下一轮该槽 fence 等完（= 上轮提交执行完毕）后取回
//! 结果。
//!
//! 时区划分（记录序 = 提交序，全按 TOP_OF_PIPE 采样——上一段的完成被段间
//! 屏障与命令序强制先于下一段的 timestamp）：
//!
//! | 位 | 采样点 | 区间含义 |
//! |---|---|---|
//! | T0 | 进场屏障后、cull dispatch 前 | —（原点） |
//! | T1 | indirect 组装屏障后、rendering 前 | compute 区间（cull+遮挡+组装+间隙） |
//! | T2 | cmd_end_rendering 后 | 图形区间（清屏+场景+UI+间隙） |
//! | T3 | Hi-Z 归约链后（6.1.2） | 金字塔构建区间（深度屏障+逐级归约+间隙） |
//! | T4 | end_command_buffer 前 | 全帧 GPU 区间（T0→T4） |
//!
//! 支持性前提不硬校验出局：timestamp 是诊断设施非渲染正确性，队列族
//! `timestamp_valid_bits == 0` 时降级停用（warn 一次），CPU 半边照常工作。
//!
//! [`PerfLedger`] 是帧样本账（CPU 各段 + GPU 区间 + 规模读数，环形窗口），
//! 分位数按需现算（排序副本取最近秩），BRP `perf_stats`/`perf_samples` 与
//! egui 面板都从这里取数。读数只进计量：任何系统不得拿帧时间决定画什么
//!（诊断性回读边界照 5.2 拍板）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use ash::{vk, Device};
use bevy::log::warn;

use crate::common::error::Result;

use super::Context;

/// 每帧槽查询位数量（T0..T4，见模块注释时区表；6.1.2 起加 Hi-Z 归约区间）。
pub const QUERIES_PER_FRAME: u32 = 5;

/// 一帧的计量样本：CPU 分段（统一口径 §3 传播之外的四段——wait/acquire/
/// 组装/双环/录制提交/present）+ GPU 区间 + 规模读数。全部毫秒。
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSample {
    /// 相邻两次 draw_frame 起点的墙钟间隔（= Update 调度 + winit + draw_frame
    /// 全帧）——draw_frame 内部各段之外的"未计时区"读数（F26 方向的仪器）。
    pub frame_wall_ms: f32,
    pub wait_slot_ms: f32,
    pub acquire_ms: f32,
    /// DrawList 槽位枚举组装（`drawlist::assemble`）。
    pub assemble_ms: f32,
    /// 双环写入（模型环增量 + 材质环全量，O(N) 走访在这里——判定线 3 读数源）。
    pub rings_ms: f32,
    /// 录制 + 提交（`record_frame` 全程含 queue_submit）。
    pub record_submit_ms: f32,
    /// present 调用本体（不等同上屏，3.2 计时注意）。
    pub present_ms: f32,
    /// 上述 CPU 各段合计。
    pub cpu_total_ms: f32,
    pub gpu_compute_ms: f32,
    pub gpu_graphics_ms: f32,
    /// Hi-Z 金字塔构建区间（深度屏障+逐级归约，6.1.2——A/B 账的构建开销单列）。
    pub gpu_hiz_ms: f32,
    /// T0→T4 全帧 GPU 区间。
    pub gpu_total_ms: f32,
    /// 规模读数：在场行数（= GPU 命令条数）与剔除后可见数。
    pub instance_rows: u32,
    pub visible_rows: u32,
}

/// 帧样本环形账（draw_frame 每帧 push，取样时排序副本取分位数）。
///
/// `Default` 必须等于 [`Self::new`]（宿主 `init_resource` 走 Default 装配）：
/// capacity 归 0 时 `push` 的裁剪条件永不成立，环形账退化无界增长，调试窗
/// 的逐帧分位数排序随窗口线性变慢（7.5 FPS 恶化根因，实抓实修）。
#[derive(bevy::prelude::Resource)]
pub struct PerfLedger {
    samples: VecDeque<FrameSample>,
    capacity: usize,
    /// 上一帧 draw_frame 起点（frame_wall_ms 分母；仅首帧为 None 记 0）。
    last_frame_start: Option<std::time::Instant>,
}

impl Default for PerfLedger {
    /// 与 [`PerfLedger::new`] 全等——无界账教训见结构体注释。
    fn default() -> Self {
        Self::new()
    }
}

impl PerfLedger {
    /// 环形窗口容量：600 帧采样窗（10s@60）× 3 档轮换仍远小于上限。
    const CAPACITY: usize = 2048;

    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(Self::CAPACITY),
            capacity: Self::CAPACITY,
            last_frame_start: None,
        }
    }

    /// draw_frame 开头调用：与上一帧起点的墙钟间隔入账并刷新起点（首帧 0）。
    pub fn mark_frame_start(&mut self) -> f32 {
        let now = std::time::Instant::now();
        let wall =
            self.last_frame_start.map_or(0.0, |t| (now - t).as_secs_f32() * 1000.0);
        self.last_frame_start = Some(now);
        wall
    }

    pub fn push(&mut self, sample: FrameSample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 全窗口样本快照（perf_samples 的导出口；调用方按需再截断）。
    pub fn snapshot(&self) -> Vec<FrameSample> {
        self.samples.iter().copied().collect()
    }

    /// 某字段的分位数（最近秩：排序后取 ceil(p×n)-1 下标）。字段由
    /// 抽取闭包给出（cpu_total/gpu_total/…逐项取）。
    pub fn percentile(&self, p: f32, field: impl Fn(&FrameSample) -> f32) -> Option<f32> {
        let mut values: Vec<f32> = self.samples.iter().map(field).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).expect("毫秒计时无 NaN"));
        let rank = ((p * values.len() as f32).ceil() as usize).clamp(1, values.len());
        Some(values[rank - 1])
    }

    /// 同一字段多分位数一次读：一遍 clone + 一趟排序取全部秩（调试窗每帧
    /// 三分位数 × 三字段的重复排序口——每帧九次全量 sort 是 7.5 恶化修复后
    /// 的残余热点，收敛为每字段一次）。
    pub fn percentiles(
        &self,
        ps: &[f32],
        field: impl Fn(&FrameSample) -> f32,
    ) -> Vec<Option<f32>> {
        let mut values: Vec<f32> = self.samples.iter().map(field).collect();
        if values.is_empty() {
            return vec![None; ps.len()];
        }
        values.sort_by(|a, b| a.partial_cmp(b).expect("毫秒计时无 NaN"));
        ps.iter()
            .map(|&p| {
                let rank =
                    ((p * values.len() as f32).ceil() as usize).clamp(1, values.len());
                Some(values[rank - 1])
            })
            .collect()
    }
}

/// GPU timestamp 计时器：每帧槽一只 query pool，槽内 4 位轮转写入。
#[derive(bevy::prelude::Resource)]
pub struct GpuTimer {
    device: Device,
    pools: Vec<vk::QueryPool>,
    /// 设备 `timestampPeriod`（ns/刻度），换算毫秒的系数。
    period_ns: f32,
    /// 各槽位是否已经历过一轮 record（首帧前的空槽 fetch 直接 None——
    /// reset 过但从未执行的查询取结果回 VK_NOT_READY，不进取数路径）。
    /// AtomicBool：Resource 须 Sync（bevy Resource 约束），原子布尔置位
    /// 足够（单线程录制，无竞争面）。
    recorded: [AtomicBool; 2],
    /// 支持性闸门结果（false = 队列族无 timestamp，全机 GPU 半边停用）。
    pub enabled: bool,
}

impl GpuTimer {
    /// 建计时器：队列族 timestamp 支持检查 → 每帧槽一只 TIMESTAMP query pool。
    /// 不支持时 `enabled=false` 空手而归（降级停用，不阻断渲染链）。
    ///
    /// # Errors
    /// query pool 创建失败（渲染链级资源，Tier② 语义由调用方分流）。
    pub fn new(ctx: &Context) -> Result<Self> {
        let valid_bits = unsafe {
            ctx.instance
                .get_physical_device_queue_family_properties(ctx.physical_device)
                [ctx.queue_family_index as usize]
                .timestamp_valid_bits
        };
        let period_ns = ctx.limits().timestamp_period;
        let enabled = valid_bits > 0;
        if !enabled {
            warn!(
                "图形队列族 {valid_bits} 个 timestamp 有效位：GPU 帧计时降级停用（CPU 分段照常），\
                 统一口径 §3 的 GPU 区间读数将缺席"
            );
        }
        // 新建查询的初始状态是"未初始化"（VUID-09401 不许直接取结果）——
        // 不做创建期 host reset（那要 hostQueryReset feature 位，VUID-02665
        // 首跑实抓），而是靠 recorded 闸门：每槽首次取结果必在其首次
        // record（cmd_reset_query_pool 起周期）之后，未初始化态不可达。
        let pools = if enabled {
            (0..super::MAX_FRAMES_IN_FLIGHT)
                .map(|_| {
                    unsafe {
                        ctx.device.create_query_pool(
                            &vk::QueryPoolCreateInfo::default()
                                .query_type(vk::QueryType::TIMESTAMP)
                                .query_count(QUERIES_PER_FRAME),
                            None,
                        )
                    }
                    .map_err(crate::common::error::VulkanError::from)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        bevy::log::info!(
            "GPU 计时就绪：每帧槽 5 位 timestamp（{valid_bits} 有效位，period {period_ns} ns）——\
             compute/graphics/Hi-Z/全帧四区间随 fence 取回"
        );
        Ok(Self {
            device: ctx.device.clone(),
            pools,
            period_ns,
            recorded: [AtomicBool::new(false), AtomicBool::new(false)],
            enabled,
        })
    }

    /// 帧首：重置本槽查询位并写 T0（record_frame 进场屏障后调用）。
    pub fn reset_and_start(&self, command_buffer: vk::CommandBuffer, slot: usize) {
        if !self.enabled {
            return;
        }
        self.recorded[slot].store(true, Ordering::Relaxed);
        unsafe {
            self.device.cmd_reset_query_pool(
                command_buffer,
                self.pools[slot],
                0,
                QUERIES_PER_FRAME,
            );
            self.device.cmd_write_timestamp(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.pools[slot],
                0,
            );
        }
    }

    /// 段界采样（T1 compute 后 / T2 rendering 后 / T3 提交前）。
    pub fn mark(&self, command_buffer: vk::CommandBuffer, slot: usize, index: u32) {
        if !self.enabled {
            return;
        }
        unsafe {
            self.device.cmd_write_timestamp(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.pools[slot],
                index,
            );
        }
    }

    /// 取本槽位上轮提交的时间戳并换算四区间（毫秒）。前提 = 本槽 fence
    /// 已等完（`wait_for_slot` 之后调用）——fence 只证提交完成。停用、该槽
    /// 从未录制过（首帧空槽）或查询无可读结果（NOT_READY）返回 None。
    ///
    /// # Errors
    /// 查询结果取回失败（设备丢失等，Tier② 语义由调用方分流）。
    pub fn fetch(&mut self, slot: usize) -> Result<Option<[f32; 4]>> {
        if !self.enabled || !self.recorded[slot].load(Ordering::Relaxed) {
            return Ok(None);
        }
        // 可用性由 fence 保证（本槽 fence 已等完 = 上轮 5 个 timestamp 都已
        // 写入），不挂 QUERY_RESULT_WAIT——WAIT 对"reset 过但未执行"的查询
        // 会永久阻塞（首跑实抓的挂死根因）。NOT_READY = 空槽漏网，按无读数
        // 处理而非失败。
        let mut raw = [0u64; QUERIES_PER_FRAME as usize];
        unsafe {
            // ash 0.38 形状：query 数由 data 切片长度隐含（无独立 count 参数）
            match self.device.get_query_pool_results(
                self.pools[slot],
                0,
                &mut raw,
                vk::QueryResultFlags::TYPE_64,
            ) {
                Ok(()) => {}
                Err(vk::Result::NOT_READY) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        // 全 0 = 该槽从未执行过 timestamp（首帧前的空槽）：不是有效读数
        if raw.iter().all(|&t| t == 0) {
            return Ok(None);
        }
        let ticks_to_ms = |a: u64, b: u64| (b.wrapping_sub(a)) as f32 * self.period_ns / 1.0e6;
        Ok(Some([
            ticks_to_ms(raw[0], raw[1]),
            ticks_to_ms(raw[1], raw[2]),
            ticks_to_ms(raw[2], raw[3]),
            ticks_to_ms(raw[0], raw[4]),
        ]))
    }
}

impl Drop for GpuTimer {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于资源 Drop 完成（帧级资源同纪律）
        for pool in &self.pools {
            unsafe { self.device.destroy_query_pool(*pool, None) };
        }
    }
}
