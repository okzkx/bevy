//! 9.7.2 patch 级空间滤波的录制束：一条 compute dispatch，每线程一条 patch。
//!
//! 描述符只两集（帧 UBO / GI 表）：滤波只碰 patch 表（几何 / 辐照度 / 统计 / cell 反查）与
//! GI UBO，不需要实例、光线求交、阴影或 bindless。
//!
//! 录制位置：④.5 段 estimation 之后（读本帧刚写的那份辐照度），9.7.3 时域滤波之前。
//! 开关关闭时仍录一条 dispatch，但 push 的 `enabled = 0` → 直拷，使 `filtered` 表始终是
//! "本帧可消费的完整一份"，下游不必再判断读哪张表（口径见 9.7.2 施工记录 §钉明）。
//!
//! 顺带一个不对称：本段（9.7.2～9.7.5）只要 estimation 开着就录；9.7.6 接上 `scgi` 开关后，
//! 整条屏幕链（含本滤波）改由 `scgi` 把关，"关 = 基线且零新增命令"才成立。

use ash::{vk, Device};

use crate::common::bytes::{put_f32, put_u32s};
use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::compute::{ComputeHandles, Handoff};

/// 空间滤波 push 字节：enabled / taps / frame_index / capacity / radius_cells / 填充。
pub const GI_SPATIAL_FILTER_PUSH_SIZE: u32 = 24;

/// 空间滤波 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct GiSpatialFilterDraw {
    pub pass: ComputeHandles,
    /// 24B push（偏移见 [`pack_spatial_filter_push`]）。
    pub push: [u8; GI_SPATIAL_FILTER_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(池容量 / 64)。
    pub groups: u32,
    /// 帧 UBO（与其它 compute 的集序一致）。
    pub frame_set: DescSet,
    /// GI 集（patch 表 + GI UBO + `filtered` 输出）。
    pub gi_set: DescSet,
}

impl GiSpatialFilterDraw {
    /// 录一次空间滤波 dispatch（set 序 = 帧 UBO / GI），后接 compute → compute 交接。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.frame_set, self.gi_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 空间滤波 push：开关 / 样本数 / 帧号 / 池容量 / 半径倍数（体素单位）。
#[must_use]
pub fn pack_spatial_filter_push(
    enabled: bool,
    taps: u32,
    frame_index: u32,
    capacity: u32,
    radius_cells: f32,
) -> [u8; GI_SPATIAL_FILTER_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_SPATIAL_FILTER_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[u32::from(enabled), taps, frame_index, capacity],
    );
    put_f32(&mut push, 16, radius_cells);
    push
}

/// 时域滤波 push 字节：enabled / capacity / frame_index / reserved / smoothing / 填充。
pub const GI_TEMPORAL_FILTER_PUSH_SIZE: u32 = 24;

/// 时域滤波 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct GiTemporalFilterDraw {
    pub pass: ComputeHandles,
    /// 24B push（偏移见 [`pack_temporal_filter_push`]）。
    pub push: [u8; GI_TEMPORAL_FILTER_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(池容量 / 64)。
    pub groups: u32,
    /// 帧 UBO（与其它 compute 的集序一致）。
    pub frame_set: DescSet,
    /// GI 集（统计表 + `filtered` 两张表）。
    pub gi_set: DescSet,
}

impl GiTemporalFilterDraw {
    /// 录一次时域滤波 dispatch（set 序 = 帧 UBO / GI），后接 compute → compute 交接。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.frame_set, self.gi_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 时域滤波 push：开关 / 池容量 / 帧号 / 平滑度。
#[must_use]
pub fn pack_temporal_filter_push(
    enabled: bool,
    capacity: u32,
    frame_index: u32,
    smoothing: f32,
) -> [u8; GI_TEMPORAL_FILTER_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_TEMPORAL_FILTER_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[u32::from(enabled), capacity, frame_index, 0],
    );
    put_f32(&mut push, 16, smoothing);
    push
}

/// 空间滤波 dispatch 组数 = ceil(池容量 / 64)（与淘汰同一档，线程 = 池槽位）。
#[must_use]
pub fn spatial_filter_groups(capacity: u32) -> u32 {
    u64::from(capacity).div_ceil(64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// push 布局：四个 u32 在前 16B、半径在 16B、填充在 20B，总长 24B 且 4B 对齐。
    #[test]
    fn spatial_filter_push_layout() {
        let push = pack_spatial_filter_push(true, 4, 77, 65_536, 1.5);
        assert_eq!(push.len(), 24);
        let words: Vec<u32> = push[0..16]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, vec![1, 4, 77, 65_536]);
        let radius = f32::from_le_bytes([push[16], push[17], push[18], push[19]]);
        assert!((radius - 1.5).abs() < f32::EPSILON);
        assert_eq!(&push[20..24], &[0, 0, 0, 0]);
    }

    /// 关态 push 的 enabled 必须是 0（下游靠它判"直拷"）。
    #[test]
    fn disabled_push_is_zero_flag() {
        let push = pack_spatial_filter_push(false, 4, 3, 1024, 2.0);
        assert_eq!(u32::from_le_bytes([push[0], push[1], push[2], push[3]]), 0);
    }

    /// 时域 push 布局：三个 u32 + 保留字在前 16B、平滑度在 16B、填充 20B。
    #[test]
    fn temporal_filter_push_layout() {
        let push = pack_temporal_filter_push(true, 65_536, 9, 0.75);
        assert_eq!(push.len(), 24);
        let words: Vec<u32> = push[0..16]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, vec![1, 65_536, 9, 0]);
        let smoothing = f32::from_le_bytes([push[16], push[17], push[18], push[19]]);
        assert!((smoothing - 0.75).abs() < f32::EPSILON);
        assert_eq!(&push[20..24], &[0, 0, 0, 0]);
    }

    /// 组数：65536 槽 → 1024 组；非整倍向上取整。
    #[test]
    fn groups_round_up() {
        assert_eq!(spatial_filter_groups(65_536), 1024);
        assert_eq!(spatial_filter_groups(1), 1);
        assert_eq!(spatial_filter_groups(65), 2);
    }
}
