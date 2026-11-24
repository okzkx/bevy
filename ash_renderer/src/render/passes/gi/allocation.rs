//! 屏幕播种的录制束（9.5.3）：1/4 网格 dispatch 的 push/组数与四集绑定。
//!
//! 种子取 IBL 基线（9.2 SH-L2 或 9.3 16 桶，按帧 UBO 的 ibl_mode），本段没有低分辨率
//! 辐照度生产者，先验等价于"缓存没数据 = 环境光"。分配/刷新的语义在
//! `shaders/scene/gi/patch_alloc.wgsl` 的文件头。

use ash::{vk, Device};

use crate::common::bytes::put_u32s;
use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::compute::{ComputeHandles, Handoff};

/// 播种 push 字节：low_w / low_h / full_w / full_h / frame_index / normal_source / pad ×2。
pub const GI_ALLOC_PUSH_SIZE: u32 = 32;

/// 播种 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct GiAllocDraw {
    pub pass: ComputeHandles,
    /// 32B push（偏移见 [`pack_alloc_push`]）。
    pub push: [u8; GI_ALLOC_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(1/4 像素数 / 64)。
    pub groups: u32,
    /// 每帧 UBO（inv_view_proj / ibl_*）。
    pub frame_set: DescSet,
    /// G-buffer 采样集（b1 normal / b3 depth）。
    pub gbuffer_set: DescSet,
    /// IBL 集（b3 SH 系数 / b4 16 桶）。
    pub ibl_set: DescSet,
}

impl GiAllocDraw {
    /// 录一次播种 dispatch（set 序 = 帧 UBO / G-buffer / IBL / GI）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer, gi_set: DescSet) {
        self.pass.record(
            device,
            cb,
            &[self.frame_set, self.gbuffer_set, self.ibl_set, gi_set],
            &self.push,
            self.groups,
            Handoff::ComputeFragmentRead,
        );
    }
}

/// 播种 push：低分辨率尺寸 / 全分辨率尺寸 / 帧号 / 法线来源。
#[must_use]
pub fn pack_alloc_push(
    low: vk::Extent2D,
    full: vk::Extent2D,
    frame_index: u32,
    normal_source: u32,
) -> [u8; GI_ALLOC_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_ALLOC_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[
            low.width,
            low.height,
            full.width,
            full.height,
            frame_index,
            normal_source,
            0,
            0,
        ],
    );
    push
}

/// 1/4 播种网格：低分辨率尺寸与 dispatch 组数。
pub fn alloc_grid(extent: vk::Extent2D) -> (vk::Extent2D, u32) {
    let low = vk::Extent2D {
        width: extent.width.max(1).div_ceil(4),
        height: extent.height.max(1).div_ceil(4),
    };
    let pixels = u64::from(low.width) * u64::from(low.height);
    (low, pixels.div_ceil(64) as u32)
}

/// 滚动 dispatch 组数 = ceil(级联数 × res³ / 64)。
#[must_use]
pub fn scrolling_groups(cascades: u32, res: u32) -> u32 {
    let threads = u64::from(cascades) * u64::from(res).pow(3);
    threads.div_ceil(64) as u32
}

/// 淘汰 dispatch 组数 = ceil(池容量 / 64)。
#[must_use]
pub fn eviction_groups(capacity: u32) -> u32 {
    u64::from(capacity).div_ceil(64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_and_group_counts_round_up() {
        let (low, groups) = alloc_grid(vk::Extent2D {
            width: 1920,
            height: 1080,
        });
        assert_eq!(low.width, 480);
        assert_eq!(low.height, 270);
        assert_eq!(groups, (480 * 270u32).div_ceil(64));
        // 非 4 倍宽高向上取整
        let (odd, _) = alloc_grid(vk::Extent2D {
            width: 1919,
            height: 1079,
        });
        assert_eq!((odd.width, odd.height), (480, 270));
        assert_eq!(scrolling_groups(4, 32), (131_072u32).div_ceil(64));
        assert_eq!(eviction_groups(65_536), 1024);
    }

    #[test]
    fn alloc_push_offsets() {
        let push = pack_alloc_push(
            vk::Extent2D {
                width: 480,
                height: 270,
            },
            vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            77,
            1,
        );
        let word = |i: usize| u32::from_le_bytes(push[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([word(0), word(1), word(2), word(3)], [480, 270, 1920, 1080]);
        assert_eq!(word(4), 77);
        assert_eq!(word(5), 1);
    }
}
