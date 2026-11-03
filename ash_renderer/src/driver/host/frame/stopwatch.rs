//! 帧秒表：CPU 分段 Instant 计时 + GPU 帧计时取回 + 帧样本入账。
//! 现场用法：帧起一行 start，各段结束点各一行 stop，帧末 push 入账。

use std::time::Instant;

use bevy::ecs::error::BevyError;

use crate::common::syntax::ResultTierExt;
use crate::render::{FrameSample, GpuTimer, PerfLedger};

/// 帧秒表（`frame_body` 的局部）：CPU 六段计时 + GPU 八段读数，帧末一次入账。
///
/// 段落严格按帧内执行顺序起止；让路帧（最小化 / acquire 过时）走不到 [`Self::push`]，
/// 不入账。
pub(super) struct FrameStopwatch {
    /// 帧起始墙钟时刻（perf.mark_frame_start 结果）。
    frame_wall_ms: f32,
    /// 当前段起点，各段结束都相对它。
    seg: Instant,
    wait_slot_ms: f32,
    acquire_ms: f32,
    assemble_ms: f32,
    rings_ms: f32,
    record_submit_ms: f32,
    present_ms: f32,
    /// GPU 各段毫秒（本槽两帧前的提交，稳态下与当前帧同构；停用时恒 0）。
    gpu_sections: [f32; 8],
}

impl FrameStopwatch {
    /// 帧起：记墙钟时刻，开启第一段（帧槽等待）。
    pub(super) fn start(perf: &mut PerfLedger) -> Self {
        Self {
            frame_wall_ms: perf.mark_frame_start(),
            seg: Instant::now(),
            wait_slot_ms: 0.0,
            acquire_ms: 0.0,
            assemble_ms: 0.0,
            rings_ms: 0.0,
            record_submit_ms: 0.0,
            present_ms: 0.0,
            gpu_sections: [0.0; 8],
        }
    }

    /// 帧槽等待段收口 + GPU 帧计时取回（本槽 fence 已等完，timestamp 可读）。
    pub(super) fn wait_slot_done(&mut self, timer: &mut GpuTimer, slot: usize) -> Result<(), BevyError> {
        self.wait_slot_ms = self.lap();
        self.gpu_sections = timer
            .fetch(slot)
            .or_fatal("GPU 帧计时取回失败，渲染链无法继续")?
            .unwrap_or([0.0; 8]);
        Ok(())
    }

    /// acquire 段收口。
    pub(super) fn acquire_done(&mut self) {
        self.acquire_ms = self.lap();
    }

    /// DrawList 组装段收口。
    pub(super) fn assemble_done(&mut self) {
        self.assemble_ms = self.lap();
    }

    /// 帧槽环写入段收口。
    pub(super) fn rings_done(&mut self) {
        self.rings_ms = self.lap();
    }

    /// 录制/提交段收口。
    pub(super) fn record_done(&mut self) {
        self.record_submit_ms = self.lap();
    }

    /// present 段收口。
    pub(super) fn present_done(&mut self) {
        self.present_ms = self.lap();
    }

    /// 帧样本入账（只进计量，不驱动绘制）。
    pub(super) fn push(self, perf: &mut PerfLedger, instance_rows: u32, visible_rows: u32) {
        let cpu_total_ms = self.wait_slot_ms + self.acquire_ms + self.assemble_ms + self.rings_ms
            + self.record_submit_ms + self.present_ms;
        perf.push(FrameSample {
            frame_wall_ms: self.frame_wall_ms,
            wait_slot_ms: self.wait_slot_ms,
            acquire_ms: self.acquire_ms,
            assemble_ms: self.assemble_ms,
            rings_ms: self.rings_ms,
            record_submit_ms: self.record_submit_ms,
            present_ms: self.present_ms,
            cpu_total_ms,
            gpu_compute_ms: self.gpu_sections[0],
            gpu_shadow_ms: self.gpu_sections[1],
            gpu_gbuffer_ms: self.gpu_sections[2],
            gpu_lighting_ms: self.gpu_sections[3],
            gpu_hiz_ms: self.gpu_sections[4],
            gpu_present_ms: self.gpu_sections[5],
            gpu_total_ms: self.gpu_sections[7], // [6] = 尾段，不入账
            instance_rows,
            visible_rows,
        });
    }

    /// 收当前段：取相对段起点的毫秒数并重标段起点。
    fn lap(&mut self) -> f32 {
        let ms = ms_since(self.seg);
        self.seg = Instant::now();
        ms
    }
}

/// 自 `start` 起的毫秒数（帧样本 CPU 分段）。
fn ms_since(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
}
