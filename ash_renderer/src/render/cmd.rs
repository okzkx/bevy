//! 命令录制标准件：图像布局屏障、全幅 viewport/scissor、图形管线绑定、顶点/索引
//! 池绑定、全屏三角形 pass。只收"怎么录"的样板；录什么（阶段/访问/布局/附件）
//! 由调用现场逐项写明，屏障的同步证据（谁罩谁）注释留在调用现场。
//!
//! 全部入口是 `unsafe fn`，与 ash 的 `cmd_*` 同契约：`cb` 处于录制态，传入句柄
//! 合法且存活到命令执行完毕。

use std::slice;

use ash::{vk, Device};

/// 一条单 mip、单 layer（base 0）的图像布局屏障；队列族固定 IGNORED（无所有权
/// 转移）。每条独立录成一次 `vkCmdPipelineBarrier`，stage 掩码即本条的 src/dst。
#[derive(Clone, Copy)]
pub(crate) struct ImageBarrier {
    pub image: vk::Image,
    pub aspect: vk::ImageAspectFlags,
    pub old_layout: vk::ImageLayout,
    pub new_layout: vk::ImageLayout,
    pub src_stage: vk::PipelineStageFlags,
    pub src_access: vk::AccessFlags,
    pub dst_stage: vk::PipelineStageFlags,
    pub dst_access: vk::AccessFlags,
}

impl ImageBarrier {
    /// 颜色目标弃旧内容进附件：UNDEFINED→COLOR_ATTACHMENT_OPTIMAL；src 访问为空
    ///（只等 `src_stage` 执行完，无内存依赖），dst = COLOR_ATTACHMENT_OUTPUT 写。
    pub(crate) fn discard_to_color(image: vk::Image, src_stage: vk::PipelineStageFlags) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::UNDEFINED,
            new_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            src_stage,
            src_access: vk::AccessFlags::empty(),
            dst_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            dst_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        }
    }

    /// 颜色附件写完转片元采样：COLOR_ATTACHMENT_OPTIMAL→SHADER_READ_ONLY_OPTIMAL，
    /// COLOR_ATTACHMENT_OUTPUT 的写 → FRAGMENT_SHADER 的读。
    pub(crate) fn color_to_sampled(image: vk::Image) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            src_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            src_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            dst_access: vk::AccessFlags::SHADER_READ,
        }
    }
}

/// 录一条图像屏障（见 [`ImageBarrier`]）。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn image_barrier(device: &Device, cb: vk::CommandBuffer, b: &ImageBarrier) {
    unsafe { image_barrier_with_memory(device, cb, b, &[]) };
}

/// 图像屏障与全局内存屏障同一次 `vkCmdPipelineBarrier` 录出（stage 掩码取
/// `b` 的 src/dst，须同时覆盖 `memory` 的访问）。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn image_barrier_with_memory(
    device: &Device,
    cb: vk::CommandBuffer,
    b: &ImageBarrier,
    memory: &[vk::MemoryBarrier<'_>],
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(b.old_layout)
        .new_layout(b.new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(b.image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(b.aspect)
                .level_count(1)
                .layer_count(1),
        )
        .src_access_mask(b.src_access)
        .dst_access_mask(b.dst_access);
    unsafe {
        device.cmd_pipeline_barrier(
            cb,
            b.src_stage,
            b.dst_stage,
            vk::DependencyFlags::empty(),
            memory,
            &[],
            slice::from_ref(&barrier),
        );
    }
}

/// 原点起、覆盖整个 `extent` 的矩形（renderArea / scissor）。
pub(crate) fn full_rect(extent: vk::Extent2D) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }
}

/// 全幅 viewport（正高度直出，深度 [0,1]；朝向定案见 graphics_pipeline 模块注释）。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn set_viewport(device: &Device, cb: vk::CommandBuffer, extent: vk::Extent2D) {
    unsafe {
        device.cmd_set_viewport(
            cb,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
    }
}

/// 全幅 viewport + 全幅 scissor（先 viewport 后 scissor）。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn set_viewport_scissor(
    device: &Device,
    cb: vk::CommandBuffer,
    extent: vk::Extent2D,
) {
    unsafe {
        set_viewport(device, cb, extent);
        device.cmd_set_scissor(cb, 0, &[full_rect(extent)]);
    }
}

/// 绑图形管线 + 从 set 0 起连续绑 `sets`（无动态偏移）。描述符绑定随 layout
/// 对象，换管线后须按新 layout 重绑。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn bind_graphics(
    device: &Device,
    cb: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    sets: &[vk::DescriptorSet],
) {
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, layout, 0, sets, &[]);
    }
}

/// 顶点/索引 buffer 整绑（binding 0、offset 0，U32 索引）；逐 draw 的定位走
/// vertexOffset/firstIndex。
///
/// # Safety
/// 同模块契约。
pub(crate) unsafe fn bind_vertex_index(
    device: &Device,
    cb: vk::CommandBuffer,
    vertex: vk::Buffer,
    index: vk::Buffer,
) {
    unsafe {
        device.cmd_bind_vertex_buffers(cb, 0, &[vertex], &[0]);
        device.cmd_bind_index_buffer(cb, index, 0, vk::IndexType::UINT32);
    }
}

/// 全屏三角形 pass 的录制输入：单 color 附件（storeOp STORE，无深度）+ 管线 +
/// 从 set 0 起绑定的集。
pub(crate) struct FullscreenPass<'a> {
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    /// Some = loadOp CLEAR 清成此色；None = loadOp DONT_CARE（全像素覆写）。
    pub clear: Option<[f32; 4]>,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub sets: &'a [vk::DescriptorSet],
}

/// 录一个完整全屏 pass：[`record_fullscreen_open`] + end_rendering。
///
/// # Safety
/// 同模块契约；`pass.view` 处于 COLOR_ATTACHMENT_OPTIMAL。
pub(crate) unsafe fn record_fullscreen(device: &Device, cb: vk::CommandBuffer, pass: &FullscreenPass<'_>) {
    unsafe {
        record_fullscreen_open(device, cb, pass);
        device.cmd_end_rendering(cb);
    }
}

/// begin_rendering → 绑管线与集 → 全幅 viewport/scissor → 全屏三角形 draw，
/// **不结束**渲染实例（调用方在同实例接画后自行 end_rendering）。全屏三角形
/// 无顶点 buffer，VS 按 vertex_index 直出。
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
        device.cmd_draw(cb, 3, 1, 0, 1);
    }
}

/// 中间目标的一轮"写后采样"：`image`（`pass.view` 的本体）UNDEFINED 进附件
///（[`ImageBarrier::discard_to_color`]，等 `entry_src_stage`）→ 全屏 pass →
/// 转片元采样（[`ImageBarrier::color_to_sampled`]）。UNDEFINED 进 = 内容每轮
/// 重写、从任意终态合法。
///
/// # Safety
/// 同模块契约。
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
