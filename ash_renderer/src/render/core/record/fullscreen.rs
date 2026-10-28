//! 全屏 pass 的录制：开始渲染、绑定、设 viewport/scissor、画一个全屏三角形；以及
//! 中间目标“先写后采样”的整轮（进出附件的两道屏障）。

use std::slice;

use ash::{vk, Device};

use crate::render::core::record::barrier::{image_barrier, ImageBarrier};
use crate::render::core::record::graphics::{bind_graphics, full_rect, set_viewport_scissor};

/// 全屏三角形 pass 的参数：一个颜色附件（写完保留，无深度）、管线、从 set 0 起绑定的描述符集。
pub(crate) struct FullscreenPass<'a> {
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    /// `Some` 先清成这个颜色；`None` 不清也不保留旧内容，前提是这个 pass 会写满每个像素。
    pub clear: Option<[f32; 4]>,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub sets: &'a [vk::DescriptorSet],
}

/// 录一个完整的全屏 pass（[`record_fullscreen_open`] 之后结束渲染）。
///
/// # Safety
/// `cb` 处于录制状态，句柄合法且活到命令执行完；`pass.view` 须已处于 COLOR_ATTACHMENT_OPTIMAL 布局。
pub(crate) unsafe fn record_fullscreen(device: &Device, cb: vk::CommandBuffer, pass: &FullscreenPass<'_>) {
    unsafe {
        record_fullscreen_open(device, cb, pass);
        device.cmd_end_rendering(cb);
    }
}

/// 开始渲染、绑定、设 viewport/scissor、画全屏三角形，但**不结束**渲染：
/// 调用方可以在同一次渲染里接着画别的，最后自己调 `cmd_end_rendering`。
///
/// # Safety
/// 同 [`record_fullscreen`]。
pub(crate) unsafe fn record_fullscreen_open(
    device: &Device,
    cb: vk::CommandBuffer,
    pass: &FullscreenPass<'_>,
) {
    let attachment = vk::RenderingAttachmentInfo::default()
        .image_view(pass.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .store_op(vk::AttachmentStoreOp::STORE);
    let attachment = match pass.clear {
        Some(float32) => attachment
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32 } }),
        None => attachment.load_op(vk::AttachmentLoadOp::DONT_CARE),
    };
    unsafe {
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(full_rect(pass.extent))
                .layer_count(1)
                .color_attachments(slice::from_ref(&attachment)),
        );
        bind_graphics(device, cb, pass.pipeline, pass.layout, pass.sets);
        set_viewport_scissor(device, cb, pass.extent);
        // 3 个顶点、无顶点 buffer：VS 按 vertex_index 生成一个盖满视口的三角形
        device.cmd_draw(cb, 3, 1, 0, 1);
    }
}

/// 中间目标"先写后采样"的一整轮：屏障丢弃旧内容进附件（等 `entry_src_stage`）→ 全屏 pass → 屏障转可采样。
/// 内容每轮都重写，所以不管 `image` 上一轮停在什么布局，从 UNDEFINED 进都合法。
///
/// # Safety
/// 同 [`record_fullscreen`]；`image` 是 `pass.view` 所属的图像。
pub(crate) unsafe fn record_fullscreen_sampled(
    device: &Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    entry_src_stage: vk::PipelineStageFlags,
    pass: &FullscreenPass<'_>,
) {
    unsafe {
        image_barrier(device, cb, &ImageBarrier::discard_to_color(image, entry_src_stage));
        record_fullscreen(device, cb, pass);
        image_barrier(device, cb, &ImageBarrier::color_to_sampled(image));
    }
}
