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
            &[
                self.gi_set,
                self.instance_set,
                self.ray_set,
                self.texture_set,
            ],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 清单 push：条数 + 活灯数。
#[must_use]
pub fn pack_light_sampling_push(
    sample_count: u32,
    light_count: u32,
) -> [u8; GI_LIGHT_SAMPLING_PUSH_SIZE as usize] {
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
            &[
                self.gi_set,
                self.instance_set,
                self.ray_set,
                self.texture_set,
            ],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 自发光重建 push：对象表容量。
#[must_use]
pub fn pack_emissive_build_push(
    object_capacity: u32,
) -> [u8; GI_EMISSIVE_BUILD_PUSH_SIZE as usize] {
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

/// 每条样本最坏要打几条射线：半球 1 + 重连接 0/1 + 自发光阴影 0/1。
pub const RAYS_PER_SAMPLE_WORST: u32 = 3;

/// 预算换算的结果（`prepare_gi` 每帧算一次，写进 GI UBO 交给 estimation）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EstimationBudget {
    /// 本帧每 patch 的样本档（1/2/4/8 里不超过设置值的最大档，最低 1）。
    pub samples: u32,
    /// 帧稀释步长（1 = 不稀释；N > 1 时按 patch 哈希取模跳帧）。
    pub stride: u32,
}

/// 按"活跃 patch 数 × 样本档 × 每样本最坏射线数 ≤ budget"换算样本档；仍超预算就转帧稀释。
///
/// 优先级（计划 §0.6）：先保样本档（画面质量），档降到 1 还超才稀释帧率——因为降档是
/// 全场的均匀降质，而稀释只让每个 patch 慢一帧收敛。
#[must_use]
pub fn plan_estimation_budget(
    active_patches: u32,
    requested_samples: u32,
    budget: u32,
) -> EstimationBudget {
    let tiers = [8u32, 4, 2, 1];
    let mut samples = 1;
    for tier in tiers {
        if tier > requested_samples {
            continue;
        }
        let rays =
            u64::from(active_patches.max(1)) * u64::from(tier) * u64::from(RAYS_PER_SAMPLE_WORST);
        if rays <= u64::from(budget) {
            samples = tier;
            break;
        }
    }
    // 档已到 1 仍超预算：按超出的倍数做帧稀释
    let rays_at_one = u64::from(active_patches.max(1)) * u64::from(RAYS_PER_SAMPLE_WORST);
    let stride = if rays_at_one <= u64::from(budget) {
        1
    } else {
        (rays_at_one.div_ceil(u64::from(budget.max(1)))).min(64) as u32
    };
    EstimationBudget { samples, stride }
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
    fn budget_prefers_sample_tier_before_dilution() {
        // 3320 条 patch、预算 262144：2 档要 19920 条射线，宽裕 → 用设置值
        let plan = plan_estimation_budget(3320, 2, 262_144);
        assert_eq!(
            plan,
            EstimationBudget {
                samples: 2,
                stride: 1
            }
        );
        // 8 档要 79680，仍宽裕
        let plan = plan_estimation_budget(3320, 8, 262_144);
        assert_eq!(
            plan,
            EstimationBudget {
                samples: 8,
                stride: 1
            }
        );
        // 活跃数压到预算下限：档位先降，不稀释
        let plan = plan_estimation_budget(50_000, 8, 262_144);
        assert_eq!(
            plan,
            EstimationBudget {
                samples: 1,
                stride: 1
            }
        );
        // 档已到 1 仍超（5 万 × 3 > 预算的一半以下）→ 稀释
        let plan = plan_estimation_budget(200_000, 8, 262_144);
        assert_eq!(plan.samples, 1);
        assert!(plan.stride >= 3, "stride={}", plan.stride);
    }

    /// 常量天空的解析自检（9.6.4 的锚点）：半球均匀采样 + 投影 + 余弦卷积折叠后，
    /// 求值式 `l0 + l1·n` 在法线方向必须等于 `πL`。
    ///
    /// 这里用与 `shaders/common/gi_sampling.wgsl` 同值的两个折叠乘数做纯算术验证——
    /// 它们是把 `(A0=π, A1=2π/3)` 与基常数 `(Y00, Y1)` 合并的结果，写错一个数量级
    /// 就会让整段估计偏亮/偏暗，而 GPU 侧只能靠读图看"像不像"。
    #[test]
    fn constant_sky_folds_to_pi_times_radiance() {
        use std::f32::consts::{PI, TAU};
        const SH_Y0: f32 = 0.282_094_79;
        const SH_Y1: f32 = 0.488_602_5;
        const FOLD_L0: f32 = 0.886_226_9;
        const FOLD_L1: f32 = 1.023_326_7;

        // 折叠乘数 = 余弦卷积系数 × 基常数
        assert!((FOLD_L0 - PI * SH_Y0).abs() < 1e-6);
        assert!((FOLD_L1 - (TAU / 3.0) * SH_Y1).abs() < 1e-6);

        // 上半个球面常量 L（下半球无光）：均匀半球采样的 1/pdf = 2π
        let radiance = 1.0f32;
        let acc_l0 = radiance * SH_Y0 * TAU;
        // 均匀半球上 ω_z 的均值是 1/2，所以 l1z 的累加量是这个的一半系数
        let acc_l1z = radiance * SH_Y1 * 0.5 * TAU;
        let stored_l0 = acc_l0 * FOLD_L0;
        let stored_l1z = acc_l1z * FOLD_L1;
        // 沿法线（n = +z）求值
        let evaluated = stored_l0 + stored_l1z;
        assert!(
            (evaluated - PI * radiance).abs() < 1e-4,
            "常量天空应求值成 πL，实际 {evaluated}"
        );
        // 两个分量各占一半：l0 与 l1·n 对称
        assert!((stored_l0 - PI * radiance * 0.5).abs() < 1e-4);
        assert!((stored_l1z - PI * radiance * 0.5).abs() < 1e-4);
    }

    #[test]
    fn light_sampling_push_layout() {
        let push = pack_light_sampling_push(128, 3);
        assert_eq!(
            u32::from_le_bytes([push[0], push[1], push[2], push[3]]),
            128
        );
        assert_eq!(u32::from_le_bytes([push[4], push[5], push[6], push[7]]), 3);
    }
}
