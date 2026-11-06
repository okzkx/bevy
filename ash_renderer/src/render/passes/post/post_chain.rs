//! ⑦⑧ 后链束：Bloom → tonemap → SMAA → neighborhood 的一次性录制输入与方法。

use std::slice;

use ash::{vk, Device};

use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::graphics::{
    bind_graphics, color_attachment, full_rect, set_viewport_scissor, PipelineRef,
};
use crate::render::passes::post::bloom::BloomFrameDraw;
use crate::render::passes::post::smaa::{SmaaEdgeBlendDraw, SmaaNbDraw};

/// ⑦⑧ 后链的录制输入（draw_frame 组装，`record_frame` 消费）。`bloom`/`smaa_edge_blend`
/// 关闭为 `None`。
pub struct PostChainDraw {
    /// tonemap 线（HDR + Bloom → ACES → LDR）。
    pub tonemap: PipelineRef,
    /// tonemap 的 bloom 参数集（总是绑；bloom 关闭时 shader 内不采样）。
    pub bloom_params_set: DescSet,
    /// Bloom 链（关闭为 None）。
    pub bloom: Option<BloomFrameDraw>,
    /// SMAA edge/blend 两 pass（关闭为 None，neighborhood 直通）。
    pub smaa_edge_blend: Option<SmaaEdgeBlendDraw>,
    /// neighborhood 混合（终点，写 swapchain，总是录）。
    pub smaa_nb: SmaaNbDraw,
}

impl PostChainDraw {
    /// 录制 ⑦⑧ 后链：Bloom → tonemap → SMAA edge/blend → neighborhood。neighborhood
    /// 开渲染不关——UI 在同一次渲染里接着画，`cmd_end_rendering` 由调用方负责。
    ///
    /// # Safety
    /// 前提由 `record_frame_steps` 统一建立（本槽 fence 已等完，cb 记录中）。
    pub fn record(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        frame_set: DescSet,
        gbuffer_sampled: DescSet,
        swapchain_view: vk::ImageView,
        extent: vk::Extent2D,
    ) {
        unsafe {
            if let Some(bloom) = &self.bloom {
                bloom.record(device, cb);
            }

            // tonemap：LDR 是 UNORM 线性存储，sRGB 编码只在最后写 swapchain 的 SRGB view
            // 时做一次。进场屏障：LDR 上一轮停在 SHADER_READ_ONLY，不录 UNDEFINED 进场会
            // 触发 VUID-09592 布局不符；出场转只读供 SMAA 采样。
            ImageBarrier::discard_to_color(
                self.smaa_nb.ldr_image,
                vk::PipelineStageFlags::TOP_OF_PIPE,
            )
            .record(device, cb);
            let ldr_attachment = color_attachment(self.smaa_nb.ldr_view, None);
            device.cmd_begin_rendering(
                cb,
                &vk::RenderingInfo::default()
                    .render_area(full_rect(self.smaa_nb.ldr_extent))
                    .layer_count(1)
                    .color_attachments(slice::from_ref(&ldr_attachment)),
            );
            bind_graphics(
                device,
                cb,
                &self.tonemap,
                &[frame_set, gbuffer_sampled, self.bloom_params_set],
            );
            set_viewport_scissor(device, cb, self.smaa_nb.ldr_extent);
            // 全屏三角形：3 个顶点、无顶点 buffer，VS 按 vertex_index 生成
            device.cmd_draw(cb, 3, 1, 0, 1);
            device.cmd_end_rendering(cb);
            ImageBarrier::color_to_sampled(self.smaa_nb.ldr_image).record(device, cb);

            // SMAA edge → blend（关闭为 None）
            if let Some(smaa) = &self.smaa_edge_blend {
                smaa.record(device, cb);
            }

            // neighborhood 合（终点）：LDR + blend → swapchain（SMAA 关闭时 shader 内直通）
            self.smaa_nb.record(device, cb, swapchain_view, extent);
        }
    }
}
