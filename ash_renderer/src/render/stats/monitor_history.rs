//! 监控历史：随 fps 快照刷新（约 0.5s）采一个点（fps/进程内存/系统内存/显存预算），环形累积。
//! overlay 的 pass 采样入账（资源由 overlay 插入）；overlay 监控页与 BRP `monitor_stats` 读同一本账。

use bevy::prelude::*;

use crate::render::Context;

/// 一个监控采样点。
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

/// 监控历史：满容量挤掉最旧；窗口统计与曲线每次从 `samples` 现算（最多 7200 点，不做增量聚合）。
#[derive(Resource, Default)]
pub(crate) struct MonitorHistory {
    /// 首个采样的时刻（elapsed 起点；None = 还没采过）。
    started: Option<std::time::Instant>,
    pub(crate) samples: Vec<MonitorSample>,
}

impl MonitorHistory {
    /// 容量：0.5s × 7200 = 1 小时。
    const CAP: usize = 7200;

    /// 采一个点：fps 快照 + 进程/系统内存 + 显存读数。overlay 在 fps 快照刚刷新时调用，与监控页
    /// 是否显示无关。
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

/// 进程工作集字节。失败静默 None——缺一行监控读数不值得报错。
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
