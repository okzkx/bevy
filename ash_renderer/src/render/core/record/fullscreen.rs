//! 全屏 pass 的录制：开始渲染、绑定、设 viewport/scissor、画一个全屏三角形；以及
//! 中间目标“先写后采样”的整轮（进出附件的两道屏障）。

use std::slice;

use ash::{vk, Device};

use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::graphics::{
    bind_graphics, color_attachment, full_rect, set_viewport_scissor, PipelineRef,
};

/// 全屏三角形 pass 的参数：一个颜色附件（写完保留，无深度）、管线、从 set 0 起绑定的描述符集。
pub(crate) struct FullscreenPass<'a> {
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    /// `Some` 先清成这个颜色；`None` 不清也不保留旧内容，前提是这个 pass 会写满每个像素。
    pub clear: Option<[f32; 4]>,
    pub pipeline: PipelineRef,
    pub sets: &'a [DescSet],
}

impl FullscreenPass<'_> {
    /// 录一个完整的全屏 pass（开渲染到关渲染；同一次渲染里接着画别的用 [`record_fullscreen_open`]）。
    ///
    /// # Safety
    /// `cb` 处于录制状态，句柄合法且活到命令执行完；`view` 须已处于 COLOR_ATTACHMENT_OPTIMAL 局。
    pub(crate) unsafe fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        unsafe {
            record_fullscreen_open(device, cb, self);
            device.cmd_end_rendering(cb);
        }
    }
}

/// 开始渲染、绑定、设 viewport/scissor、画全屏三角形，但**不结束**渲染：
/// 调用方可以在同一次渲染里接着画别的，最后自己调 `cmd_end_rendering`。
///
/// # Safety
/// 同 [`FullscreenPass::record`]。
pub(crate) unsafe fn record_fullscreen_open(
    device: &Device,
    cb: vk::CommandBuffer,
    pass: &FullscreenPass<'_>,
) {
    let attachment = color_attachment(pass.view, pass.clear);
    unsafe {
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(full_rect(pass.extent))
                .layer_count(1)
                .color_attachments(slice::from_ref(&attachment)),
        );
        bind_graphics(device, cb, &pass.pipeline, pass.sets);
        set_viewport_scissor(device, cb, pass.extent);
        // 3 个顶点、无顶点 buffer：VS 按 vertex_index 生成一个盖满视口的三角形
        device.cmd_draw(cb, 3, 1, 0, 1);
    }
}

/// 中间目标"先写后采样"的一整轮：屏障丢弃旧内容进附件（等 `entry_src_stage`）→ 全屏 pass → 屏障转可采样。
/// 内容每轮都重写，所以不管 `image` 上一轮停在什么布局，从 UNDEFINED 进都合法。
///
/// # Safety
/// 同 [`FullscreenPass::record`]；`image` 是 `pass.view` 所属的图像。
pub(crate) unsafe fn record_fullscreen_sampled(
    device: &Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    entry_src_stage: vk::PipelineStageFlags,
    pass: &FullscreenPass<'_>,
) {
    unsafe {
        ImageBarrier::discard_to_color(image, entry_src_stage).record(device, cb);
        pass.record(device, cb);
        ImageBarrier::color_to_sampled(image).record(device, cb);
    }
}
