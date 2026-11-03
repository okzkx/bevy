//! GPU timestamp 计时与帧样本账。每帧槽一只 TIMESTAMP query pool（[`QUERIES_PER_FRAME`] = 8 位），
//! 帧内写 8 个 timestamp，下一轮该槽 fence 等完后取回。
//!
//! 采样阶段：T0 用 TOP_OF_PIPE；T1..T7 用 BOTTOM_OF_PIPE，等记录序之前的命令全部走完才写。
//! 坑：段界若用 TOP_OF_PIPE，timestamp 不等前面的工作，段间屏障也约束不到它，读数会系统性
//! 偏小甚至挪到相邻段。
//!
//! | 位 | 采样点 | 区间（到上一位） |
//! |---|---|---|
//! | T0 | 进场屏障前、compute 段前 | 原点 |
//! | T1 | compute 段（剔除/遮挡/LOD/组装/光源列表/透明）后 | compute |
//! | T2 | shadow depth pass 后 | shadow |
//! | T3 | G-buffer pass 后 | G-buffer |
//! | T4 | 合成 + 透明层后 | 合成（含透明） |
//! | T5 | Hi-Z 归约后 | Hi-Z 构建 |
//! | T6 | Bloom/tonemap/SMAA/neighborhood + UI 后 | 后处理 + UI |
//! | T7 | end_command_buffer 前 | 尾段（不入账）；全帧取 T0→T7 |
//!
//! timestamp 只用于诊断：队列族 `timestamp_valid_bits == 0` 时降级停用（warn 一次），不阻断渲染。
//!
//! [`PerfLedger`] 是帧样本环形账（CPU 各段 + GPU 区间 + 规模读数），分位数按需现算。BRP
//! `perf_stats`/`perf_samples` 与 egui 面板都从这里取数。读数只用于观测，不得用帧时间决定画什么。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use ash::{vk, Device};
use bevy::log::warn;

use crate::common::error::Result;

use crate::render::Context;

fn wrapping_timestamp_delta(start: u64, end: u64, mask: u64) -> u64 {
    end.wrapping_sub(start) & mask
}

/// 每帧槽查询位数（T0..T7，见模块注释）。
pub(crate) const QUERIES_PER_FRAME: u32 = 8;

/// 一帧的计量样本：CPU 分段 + GPU 区间 + 规模读数，全部毫秒。
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameSample {
    /// 相邻两次 draw_frame 起点的墙钟间隔（含 Update 调度、winit 与 draw_frame 全部），减去
    /// 下面各段即 draw_frame 之外的耗时。
    pub frame_wall_ms: f32,
    pub wait_slot_ms: f32,
    pub acquire_ms: f32,
    /// DrawList 组装（`drawlist::assemble`）。
    pub assemble_ms: f32,
    /// 模型环增量 + 材质环全量写入。
    pub rings_ms: f32,
    /// 录制 + 提交（`record_frame` 全程）。
    pub record_submit_ms: f32,
    /// present 调用本身（不等于上屏时刻）。
    pub present_ms: f32,
    /// 上述 CPU 各段合计。
    pub cpu_total_ms: f32,
    pub gpu_compute_ms: f32,
    /// shadow depth pass（四级联）。
    pub gpu_shadow_ms: f32,
    /// G-buffer pass。
    pub gpu_gbuffer_ms: f32,
    /// 合成（光照 + 透明层）。
    pub gpu_lighting_ms: f32,
    /// Hi-Z 金字塔构建。
    pub gpu_hiz_ms: f32,
    /// 后处理 + UI（Bloom/tonemap/SMAA/neighborhood + UI）。
    pub gpu_present_ms: f32,
    /// T0→T7 全帧。
    pub gpu_total_ms: f32,
    /// 在场行数（= GPU 命令条数）与剔除后可见数。
    pub instance_rows: u32,
    pub visible_rows: u32,
}

/// 帧样本环形账（draw_frame 每帧 push，满 [`Self::CAPACITY`] 挤掉最旧）。
///
/// 坑：容量用常量而不存字段。曾经容量是字段，宿主 `init_resource` 走 Default 时归 0，裁剪
/// 条件永不成立，账无界增长，调试窗逐帧排序越来越慢，FPS 持续恶化。
#[derive(bevy::prelude::Resource)]
pub struct PerfLedger {
    samples: VecDeque<FrameSample>,
    /// 上一帧 draw_frame 起点（首帧为 None，间隔记 0）。
    last_frame_start: Option<std::time::Instant>,
}

impl Default for PerfLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfLedger {
    /// 环形窗口容量（帧）。
    const CAPACITY: usize = 2048;

    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(Self::CAPACITY),
            last_frame_start: None,
        }
    }

    /// draw_frame 开头调用：返回距上一帧起点的墙钟毫秒（首帧 0）并刷新起点。
    pub fn mark_frame_start(&mut self) -> f32 {
        let now = std::time::Instant::now();
        let wall =
            self.last_frame_start.map_or(0.0, |t| (now - t).as_secs_f32() * 1000.0);
        self.last_frame_start = Some(now);
        wall
    }

    pub fn push(&mut self, sample: FrameSample) {
        if self.samples.len() == Self::CAPACITY {
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

    /// 全窗口样本副本。
    pub fn snapshot(&self) -> Vec<FrameSample> {
        self.samples.iter().copied().collect()
    }

    /// 某字段的分位数（最近秩：排序后取第 ceil(p×n) 个）；无样本返回 None。
    pub fn percentile(&self, p: f32, field: impl Fn(&FrameSample) -> f32) -> Option<f32> {
        self.percentiles(&[p], field).pop().flatten()
    }

    /// 同一字段一次排序取多个分位数（调试窗每帧取多个，逐个调 [`Self::percentile`] 会重复排序）。
    pub fn percentiles(
        &self,
        ps: &[f32],
        field: impl Fn(&FrameSample) -> f32,
    ) -> Vec<Option<f32>> {
        let mut values: Vec<f32> = self.samples.iter().map(field).collect();
        if values.is_empty() {
            return vec![None; ps.len()];
        }
        values.sort_by(f32::total_cmp);
        ps.iter()
            .map(|&p| {
                let rank =
                    ((p * values.len() as f32).ceil() as usize).clamp(1, values.len());
                Some(values[rank - 1])
            })
            .collect()
    }
}

/// 建 query pool 途中失败时回收已建的 pool。
struct QueryPoolsBuildGuard<'a> {
    device: &'a Device,
    pools: Vec<vk::QueryPool>,
}

impl Drop for QueryPoolsBuildGuard<'_> {
    fn drop(&mut self) {
        for pool in self.pools.drain(..) {
            unsafe { self.device.destroy_query_pool(pool, None) };
        }
    }
}

/// GPU timestamp 计时器：每帧槽一只 query pool，槽内 [`QUERIES_PER_FRAME`] 位逐帧重写。
#[derive(bevy::prelude::Resource)]
pub struct GpuTimer {
    device: Device,
    pools: Vec<vk::QueryPool>,
    /// 设备 `timestampPeriod`（ns/刻度）。
    period_ns: f32,
    /// 计数器有效位掩码（硬件计数器可能早于 u64 回绕）。
    timestamp_mask: u64,
    /// 各槽是否录制过至少一轮；没录过的槽 fetch 直接返回 None。用 AtomicBool 是因为
    /// Resource 须 Sync（录制是单线程，没有竞争）。
    recorded: [AtomicBool; crate::render::MAX_FRAMES_IN_FLIGHT],
    /// false = 队列族不支持 timestamp，GPU 计时整体停用。
    pub enabled: bool,
}

impl GpuTimer {
    /// 检查队列族 timestamp 支持，每帧槽建一只 TIMESTAMP query pool；不支持时
    /// `enabled=false`，不阻断渲染。
    ///
    /// # Errors
    /// query pool 创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let valid_bits = unsafe {
            ctx.instance
                .get_physical_device_queue_family_properties(ctx.physical_device)
                [ctx.queue_family_index as usize]
                .timestamp_valid_bits
        };
        let period_ns = ctx.limits().timestamp_period;
        let enabled = valid_bits > 0;
        let timestamp_mask = match valid_bits {
            0 => 0,
            64.. => u64::MAX,
            bits => (1u64 << bits) - 1,
        };
        if !enabled {
            warn!(
                "图形队列族 {valid_bits} 个 timestamp 有效位：GPU 帧计时降级停用（CPU 分段照常），\
                 GPU 区间读数将缺席"
            );
        }
        // 坑：新建的查询处于未初始化状态，不能直接取结果（VUID-09401）；创建期 host reset 需要
        // hostQueryReset feature（否则 VUID-02665）。所以靠 recorded：每槽首次取结果必在它首次
        // 录制（cmd_reset_query_pool）之后。
        let pools = if enabled {
            let mut guard = QueryPoolsBuildGuard {
                device: &ctx.device,
                pools: Vec::with_capacity(crate::render::MAX_FRAMES_IN_FLIGHT),
            };
            for _ in 0..crate::render::MAX_FRAMES_IN_FLIGHT {
                guard.pools.push(unsafe {
                    ctx.device.create_query_pool(
                        &vk::QueryPoolCreateInfo::default()
                            .query_type(vk::QueryType::TIMESTAMP)
                            .query_count(QUERIES_PER_FRAME),
                        None,
                    )
                }?);
            }
            std::mem::take(&mut guard.pools)
        } else {
            Vec::new()
        };
        bevy::log::info!(
            "GPU 计时就绪：每帧槽 {QUERIES_PER_FRAME} 位 timestamp（{valid_bits} 有效位，period {period_ns} ns）——\
             compute/shadow/G-buffer/合成/Hi-Z/blit/尾段七区间 + 全帧随 fence 取回"
        );
        Ok(Self {
            device: ctx.device.clone(),
            pools,
            period_ns,
            timestamp_mask,
            recorded: std::array::from_fn(|_| AtomicBool::new(false)),
            enabled,
        })
    }

    /// 帧首：重置本槽查询位并写 T0（record_frame 开始录制后立即调用）。
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

    /// 段界采样 T1..T7（BOTTOM_OF_PIPE，理由见模块注释）。
    pub fn mark(&self, command_buffer: vk::CommandBuffer, slot: usize, index: u32) {
        if !self.enabled {
            return;
        }
        unsafe {
            self.device.cmd_write_timestamp(
                command_buffer,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.pools[slot],
                index,
            );
        }
    }

    /// 取本槽上一轮的时间戳，换算成 7 段（T0→T1 … T6→T7）+ 全帧 T0→T7，共 8 项毫秒。
    /// 须在本槽 fence 等完后调用（`wait_for_slot` 之后）。停用、该槽没录过或结果未就绪返回 None。
    ///
    /// # Errors
    /// 取结果失败（如设备丢失）。
    pub fn fetch(&mut self, slot: usize) -> Result<Option<[f32; 8]>> {
        if !self.enabled || !self.recorded[slot].load(Ordering::Relaxed) {
            return Ok(None);
        }
        // 坑：不加 QUERY_RESULT_WAIT——对 reset 过但没执行的查询它会永久阻塞（曾因此挂死）。
        // fence 已保证上轮结果写完；NOT_READY 按无读数处理。
        let mut raw = [0u64; QUERIES_PER_FRAME as usize];
        unsafe {
            // 查询数由 raw 的长度决定
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
        // 全 0 = 该槽从没真正执行过 timestamp，不是有效读数
        if raw.iter().all(|&t| t == 0) {
            return Ok(None);
        }
        let ticks_to_ms = |a: u64, b: u64| {
            wrapping_timestamp_delta(a, b, self.timestamp_mask) as f32 * self.period_ns / 1.0e6
        };
        Ok(Some([
            ticks_to_ms(raw[0], raw[1]),
            ticks_to_ms(raw[1], raw[2]),
            ticks_to_ms(raw[2], raw[3]),
            ticks_to_ms(raw[3], raw[4]),
            ticks_to_ms(raw[4], raw[5]),
            ticks_to_ms(raw[5], raw[6]),
            ticks_to_ms(raw[6], raw[7]),
            ticks_to_ms(raw[0], raw[7]),
        ]))
    }
}

impl Drop for GpuTimer {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle
        for pool in &self.pools {
            unsafe { self.device.destroy_query_pool(*pool, None) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wrapping_timestamp_delta;

    #[test]
    fn timestamp_delta_wraps_at_device_valid_width() {
        let mask = (1u64 << 36) - 1;
        assert_eq!(wrapping_timestamp_delta(mask - 2, 3, mask), 6);
        assert_eq!(wrapping_timestamp_delta(u64::MAX - 2, 3, u64::MAX), 6);
    }
}
