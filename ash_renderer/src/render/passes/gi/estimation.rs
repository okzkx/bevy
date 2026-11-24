//! 9.6 estimation 的录制束：一条 compute dispatch，每线程一条 patch。
//!
//! 描述符六集复用既有资源族（帧 UBO / GI 表 / 实例表 / 光线求交集 / 阴影集 / bindless），
//! 其中光线求交集直接用 9.4 那一份（b0 TLAS / b1 顶点池 / b2 索引池）——estimation 的
//! shader 不声明它的调试图与统计，管线布局按 ABI 只要求"声明的 binding 在布局里存在"，
//! 所以不需要另立一套求交集。
//!
//! 录制位置：④.5 段播种之后、段末 compute→片元屏障之前（estimation 读本帧刚刷新/分配的
//! patch 几何）。段末那道 `ComputeFragmentRead` 顺带给 ⑧ 段视图。

use ash::{vk, Device};

use crate::common::bytes::put_u32s;
use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::compute::{ComputeHandles, Handoff};

/// estimation 的录制输入（`prepare_gi` 组装，`GiFrameDraw::record` 消费）。
#[derive(Clone, Copy)]
pub struct GiEstimationDraw {
    pub pass: ComputeHandles,
    /// dispatch 组数 = ceil(池容量 / 64)。
    pub groups: u32,
    /// 帧 UBO（方向光、IBL 开关、天空参数）。
    pub frame_set: DescSet,
    /// GI 集（patch 表 + 计数 + GI UBO）。
    pub gi_set: DescSet,
    /// 实例表（命中重建要用对象记录与材质环）。
    pub instance_set: DescSet,
    /// 光线求交集（TLAS + 顶点池 + 索引池）。
    pub ray_set: DescSet,
    /// 阴影集（CSM 四级联，低档 PCF）。
    pub shadow_set: DescSet,
    /// bindless 贴图（命中点 albedo）。
    pub texture_set: DescSet,
}

impl GiEstimationDraw {
    /// 录一次 estimation dispatch（set 序 = 帧 UBO / GI / 实例 / 光线求交 / 阴影 / bindless）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[
                self.frame_set,
                self.gi_set,
                self.instance_set,
                self.ray_set,
                self.shadow_set,
                self.texture_set,
            ],
            &[],
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 光源侧采样清单的 push 字节：清单条数 + 活灯数。
pub const GI_LIGHT_SAMPLING_PUSH_SIZE: u32 = 8;

/// 光源侧采样清单的录制输入（`prepare_gi` 组装，先于 estimation 录）。
#[derive(Clone, Copy)]
pub struct GiLightSampleDraw {
    pub pass: ComputeHandles,
    /// 8B push（清单条数 / 活灯数，偏移见 [`pack_light_sampling_push`]）。
    pub push: [u8; GI_LIGHT_SAMPLING_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(清单条数 / 64)。
    pub groups: u32,
    /// GI 集（b9 UBO 帧号 / b10 光源紧凑表 / b11 本清单）。
    pub gi_set: DescSet,
    /// 实例表（命中重建）。
    pub instance_set: DescSet,
    /// 光线求交集。
    pub ray_set: DescSet,
    /// bindless 贴图（命中点反照率）。
    pub texture_set: DescSet,
}

impl GiLightSampleDraw {
    /// 录一次清单 dispatch（set 序 = GI / 实例 / 光线求交 / bindless）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.gi_set, self.instance_set, self.ray_set, self.texture_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 清单 push：条数 + 活灯数。
#[must_use]
pub fn pack_light_sampling_push(sample_count: u32, light_count: u32) -> [u8; GI_LIGHT_SAMPLING_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_LIGHT_SAMPLING_PUSH_SIZE as usize];
    put_u32s(&mut push, 0, &[sample_count, light_count]);
    push
}

/// 自发光重建的 push 字节：对象表容量。
pub const GI_EMISSIVE_BUILD_PUSH_SIZE: u32 = 4;

/// 自发光三角形表重建的录制输入（`prepare_gi` 组装，先于清单与 estimation 录）。
#[derive(Clone, Copy)]
pub struct GiEmissiveBuildDraw {
    pub pass: ComputeHandles,
    /// 4B push（对象表容量）。
    pub push: [u8; GI_EMISSIVE_BUILD_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(对象表容量 / 64)。
    pub groups: u32,
    /// GI 集（b8 计数 / b12 自发光表）。
    pub gi_set: DescSet,
    /// 实例表（对象 / 模型矩阵 / 材质环）。
    pub instance_set: DescSet,
    /// 光线求交集（顶点池 / 索引池）。
    pub ray_set: DescSet,
    /// bindless 贴图（自发光贴图）。
    pub texture_set: DescSet,
}

impl GiEmissiveBuildDraw {
    /// 录一次重建 dispatch（set 序 = GI / 实例 / 光线求交 / bindless）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.gi_set, self.instance_set, self.ray_set, self.texture_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 自发光重建 push：对象表容量。
#[must_use]
pub fn pack_emissive_build_push(object_capacity: u32) -> [u8; GI_EMISSIVE_BUILD_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_EMISSIVE_BUILD_PUSH_SIZE as usize];
    put_u32s(&mut push, 0, &[object_capacity]);
    push
}

/// bounce 分配的录制输入（`prepare_gi` 组装，录在 estimation 之后）。
#[derive(Clone, Copy)]
pub struct GiBounceAllocDraw {
    pub pass: ComputeHandles,
    /// dispatch 组数 = ceil(请求上限 / 64)（固定值，实际条数由 GPU 计数决定）。
    pub groups: u32,
    /// GI 集（请求表 + patch 表 + ring + 计数都在这一集里）。
    pub gi_set: DescSet,
}

impl GiBounceAllocDraw {
    /// 录一次分配 dispatch（单集、无 push；下游是 ⑧ 段视图与下一帧的估计）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.gi_set],
            &[],
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// bounce 分配的组数 = ceil(请求上限 / 64)。
#[must_use]
pub fn bounce_alloc_groups(capacity: u32) -> u32 {
    u64::from(capacity).div_ceil(64) as u32
}

/// 自发光重建的组数 = ceil(对象表容量 / 64)。
#[must_use]
pub fn emissive_build_groups(object_capacity: u32) -> u32 {
    u64::from(object_capacity).div_ceil(64) as u32
}

/// 清单 dispatch 组数 = ceil(条数 / 64)。
#[must_use]
pub fn light_sampling_groups(sample_count: u32) -> u32 {
    u64::from(sample_count).div_ceil(64) as u32
}

/// dispatch 组数 = ceil(池容量 / 64)（每线程一条 patch，非活跃线程直接返回）。
#[must_use]
pub fn estimation_groups(capacity: u32) -> u32 {
    u64::from(capacity).div_ceil(64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_round_up_to_workgroup() {
        assert_eq!(estimation_groups(64), 1);
        assert_eq!(estimation_groups(65), 2);
        assert_eq!(estimation_groups(65_536), 1024);
        assert_eq!(light_sampling_groups(128), 2);
        assert_eq!(light_sampling_groups(1), 1);
        assert_eq!(emissive_build_groups(131_072), 2048);
        assert_eq!(bounce_alloc_groups(4096), 64);
    }

    #[test]
    fn light_sampling_push_layout() {
        let push = pack_light_sampling_push(128, 3);
        assert_eq!(u32::from_le_bytes([push[0], push[1], push[2], push[3]]), 128);
        assert_eq!(u32::from_le_bytes([push[4], push[5], push[6], push[7]]), 3);
    }
}
