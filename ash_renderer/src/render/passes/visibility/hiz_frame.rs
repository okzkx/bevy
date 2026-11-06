//! 一帧 Hi-Z 的录制输入束：② 读两帧前金字塔做遮挡 + ⑥ 归约重写本帧槽金字塔。

use ash::{vk, Device};

use crate::render::core::binding::set_layout::DescSet;
use crate::render::passes::visibility::hiz::HizPyramid;
use crate::render::passes::visibility::occlusion::OcclusionDraw;

/// 一帧 Hi-Z 的录制输入（draw_frame 组装，`record_frame` 消费）。
pub struct HizFrameDraw<'a> {
    /// 遮挡测试（关闭/降级/空场为 None）。
    pub occlusion: Option<OcclusionDraw>,
    /// 金字塔本体。归约链每帧都录，warmup 走账与录制一一对应（录制后 note_reduced）。
    pub pyramid: &'a HizPyramid,
}

impl HizFrameDraw<'_> {
    /// ② 段：读视锥标记与两帧前的金字塔做遮挡（跳过时不录）。屏障在 [`OcclusionDraw::record`] 内。
    pub fn record_occlusion(&self, device: &Device, cb: vk::CommandBuffer, instance_set: DescSet) {
        if let Some(occlusion) = &self.occlusion {
            occlusion.record(device, cb, instance_set);
        }
    }

    /// ⑥ 段：逐级重写本帧槽金字塔。屏障在 [`HizPyramid::record_reduction`] 内。
    pub fn record_reduction(&self, device: &Device, cb: vk::CommandBuffer, slot: usize) {
        self.pyramid.record_reduction(device, cb, slot);
    }
}
