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
    }
}
