//! 一帧的反射录制束（10.3）：拷贝 → 步进 → 合成，三步按固定顺序录在同一段里。
//!
//! 三步各自的屏障也在 [`ReflectionFrameDraw::record`] 里，顺序是：
//! 1. HDR（刚被合成写完）→ 只读：拷贝与步进都要采它；
//! 2. 拷贝 pass 把 HDR 抄进本帧槽的 `hdr_copy`（写成可采样）；
//! 3. 半分辨率步进（compute 写反射图，末尾给 compute|片元读的屏障）；
//! 4. HDR 丢弃旧内容转回附件（等步进的读），反射合成覆盖写入。
//!
//! 第 4 步之后 HDR 停在 COLOR_ATTACHMENT_OPTIMAL，正好是透明层 LOAD 混合需要的布局；
//! 关档时本束为 `None`，record.rs 里连屏障都不录，帧图与 10.2 完全一致。

use ash::{Device, vk};

use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::fullscreen::{FullscreenPass, record_fullscreen_sampled};
use crate::render::core::record::graphics::PipelineRef;
use crate::render::passes::reflection::ssr_pass::SsrFrameDraw;

/// 反射三段（拷贝 / 步进 / 合成）的录制束：句柄与图像都是本帧槽的。
pub struct ReflectionFrameDraw {
    march: SsrFrameDraw,
    hdr_image: vk::Image,
    hdr_view: vk::ImageView,
    copy_image: vk::Image,
    copy_view: vk::ImageView,
    extent: vk::Extent2D,
    copy_pipeline: PipelineRef,
    composite_pipeline: PipelineRef,
    copy_set: DescSet,
    composite_sets: [DescSet; 4],
}

impl ReflectionFrameDraw {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        march: SsrFrameDraw,
        hdr_image: vk::Image,
        hdr_view: vk::ImageView,
        copy_image: vk::Image,
        copy_view: vk::ImageView,
        extent: vk::Extent2D,
        copy_pipeline: PipelineRef,
        composite_pipeline: PipelineRef,
        copy_set: DescSet,
        composite_sets: [DescSet; 4],
    ) -> Self {
        Self {
            march,
            hdr_image,
            hdr_view,
            copy_image,
            copy_view,
            extent,
            copy_pipeline,
            composite_pipeline,
            copy_set,
            composite_sets,
        }
    }

    /// 录 ④.7 整段（三步 + 三道屏障）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        unsafe {
            // 合成写 HDR → 只读：拷贝 pass 与步进都采它
            ImageBarrier::color_to_sampled(self.hdr_image).record(device, cb);
            record_fullscreen_sampled(
                device,
                cb,
                self.copy_image,
                // 等上轮片元采样读完再覆写（本帧槽的 WAR）
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                &FullscreenPass {
                    view: self.copy_view,
                    extent: self.extent,
                    clear: None,
                    pipeline: self.copy_pipeline,
                    sets: std::slice::from_ref(&self.copy_set),
                },
            );
            // 半分辨率步进；段末屏障等片元读完 HDR 与反射图
            self.march.record(device, cb);
            // HDR 丢弃旧内容转回附件（读数全来自 hdr_copy），反射合成覆盖写入
            ImageBarrier::discard_to_color(
                self.hdr_image,
                vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::COMPUTE_SHADER,
            )
            .record(device, cb);
            FullscreenPass {
                view: self.hdr_view,
                extent: self.extent,
                // 每个像素都写：基色从 hdr_copy 采样，不需要保留旧内容
                clear: None,
                pipeline: self.composite_pipeline,
                sets: &self.composite_sets,
            }
            .record(device, cb);
        }
    }
}
