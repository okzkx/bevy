//! patch 调试视图的录制束（9.5.6）：两形态都画在 ⑧ 段的 LDR 目标上、UI 之前，
//! 不经过 ACES（`gi_dump` 复用 `frame_dump` 的 LDR→PPM 通道就能取到视图）。
//!
//! - 形态 A（主）覆盖图：全屏三角形，每像素按自己的深度与平面法线算 cell/patch，
//!   DONT_CARE 进场（整屏都写满）。
//! - 形态 B 方片：一次实例化 draw（6 顶点 × 池容量），LOAD 在覆盖图之上。
//!
//! LDR 布局在 SMAA 后停在 SHADER_READ_ONLY，这里转附件、画完转回——neighborhood 接着采样。

use ash::{vk, Device};

use crate::common::bytes::put_u32s;
use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::graphics::{
    bind_graphics, color_attachment, full_rect, load_color_attachment, set_viewport_scissor,
    PipelineRef,
};

/// 视图 push 字节：视图号 / 法线来源 / 屏幕宽高。
pub const GI_VIEW_PUSH_SIZE: u32 = 16;

/// ⑧ 段 patch 视图的录制输入（视图关为 None）。
#[derive(Clone, Copy)]
pub struct GiViewDraw {
    /// 形态 A 覆盖图管线（form 2 = 只方片时为 None）。
    pub cover: Option<PipelineRef>,
    /// 形态 B 方片管线（form 0 时为 None）。
    pub marker: Option<PipelineRef>,
    /// 16B push（偏移见 [`pack_view_push`]）。
    pub push: [u8; GI_VIEW_PUSH_SIZE as usize],
    pub frame_set: DescSet,
    pub gbuffer_set: DescSet,
    pub gi_set: DescSet,
    /// 9.7.7 视图 14/15：SCGI 全分辨率辐照度采样集。
    pub scgi_set: DescSet,
    /// 方片实例数 = 池容量（未用槽输出退化三角形）。
    pub capacity: u32,
    /// LDR 后链目标（与 SMAA neighborhood 同一张；视图画在它上面）。
    pub ldr_view: vk::ImageView,
    pub ldr_image: vk::Image,
    pub extent: vk::Extent2D,
}

impl GiViewDraw {
    /// 录视图：LDR 转附件 → 覆盖图 → 方片 → 转回可采样。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        // SMAA edge/blend 采样过 LDR（SHADER_READ_ONLY）；写前转附件（LOAD 语义由各 pass 定）
        unsafe {
            ImageBarrier::transition(
                self.ldr_image,
                vk::ImageAspectFlags::COLOR,
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                ),
                (
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::COLOR_ATTACHMENT_READ
                        | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
            )
            .record(device, cb);

            let sets = [self.frame_set, self.gbuffer_set, self.gi_set, self.scgi_set];
            if let Some(cover) = &self.cover {
                // 形态 A 写满每个像素：DONT_CARE 进场
                let attachment = color_attachment(self.ldr_view, None);
                device.cmd_begin_rendering(
                    cb,
                    &vk::RenderingInfo::default()
                        .render_area(full_rect(self.extent))
                        .layer_count(1)
                        .color_attachments(std::slice::from_ref(&attachment)),
                );
                bind_graphics(device, cb, cover, &sets);
                set_viewport_scissor(device, cb, self.extent);
                device.cmd_push_constants(
                    cb,
                    cover.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &self.push,
                );
                device.cmd_draw(cb, 3, 1, 0, 1);
                device.cmd_end_rendering(cb);
            }
            if let Some(marker) = &self.marker {
                let attachment = load_color_attachment(self.ldr_view);
                device.cmd_begin_rendering(
                    cb,
                    &vk::RenderingInfo::default()
                        .render_area(full_rect(self.extent))
                        .layer_count(1)
                        .color_attachments(std::slice::from_ref(&attachment)),
                );
                bind_graphics(device, cb, marker, &sets);
                set_viewport_scissor(device, cb, self.extent);
                device.cmd_push_constants(
                    cb,
                    marker.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &self.push,
                );
                device.cmd_draw(cb, 6, self.capacity, 0, 0);
                device.cmd_end_rendering(cb);
            }

            // 转回可采样：SMAA neighborhood 读 LDR
            ImageBarrier::color_to_sampled(self.ldr_image).record(device, cb);
        }
    }
}

/// 视图 push：视图号 / 法线来源 / 宽 / 高。
#[must_use]
pub fn pack_view_push(
    view: u32,
    normal_source: u32,
    extent: vk::Extent2D,
) -> [u8; GI_VIEW_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_VIEW_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[view, normal_source, extent.width, extent.height],
    );
    push
}
