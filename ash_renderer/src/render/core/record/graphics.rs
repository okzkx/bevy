//! 图形管线的录制助手：录制句柄、绑定管线与描述符集、顶点/索引 buffer、viewport/scissor。
//! 所有 `unsafe fn` 的前提与 ash 的 `cmd_*` 相同：`cb` 处于录制状态，传入的句柄合法且
//! 活到命令执行完。

use ash::{vk, Device};

use crate::render::core::binding::set_layout::{assert_set_sequence, DescSet, SetLayoutDesc};

/// 图形管线的录制句柄（Copy）：持有方管生命周期，录制输入束带这个。
#[derive(Clone, Copy)]
pub struct PipelineRef {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 预期的 set 声明序列（建管线时从配方抄入；绑定序校验用）。
    pub sets: &'static [&'static SetLayoutDesc],
}

/// 覆盖整个 `extent` 的矩形，用作 renderArea 或 scissor。
pub(crate) fn full_rect(extent: vk::Extent2D) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }
}

/// 颜色附件：COLOR_ATTACHMENT_OPTIMAL + STORE。`Some` 先清成这个颜色；`None` = DONT_CARE，
/// 前提是这个 pass 会写满每个像素。
pub(crate) fn color_attachment(
    view: vk::ImageView,
    clear: Option<[f32; 4]>,
) -> vk::RenderingAttachmentInfo<'static> {
    let attachment = vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .store_op(vk::AttachmentStoreOp::STORE);
    match clear {
        Some(float32) => {
            attachment
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32 },
                })
        }
        None => attachment.load_op(vk::AttachmentLoadOp::DONT_CARE),
    }
}

/// 深度附件：清 0（reverse-Z 远平面）+ STORE，写完留给后续采样。
pub(crate) fn reverse_z_depth_attachment(
    view: vk::ImageView,
) -> vk::RenderingAttachmentInfo<'static> {
    vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 0.0,
                stencil: 0,
            },
        })
}

/// 颜色附件 LOAD + STORE：底子已画好（前一个 pass 的输出），在上面继续画混合进去。
pub(crate) fn load_color_attachment(view: vk::ImageView) -> vk::RenderingAttachmentInfo<'static> {
    vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
}

/// 深度附件 LOAD，只测不写：storeOp DONT_CARE 没关系——shader 没写过东西，内容不丢，
/// 后续 pass 还要采样它。
pub(crate) fn load_depth_attachment(view: vk::ImageView) -> vk::RenderingAttachmentInfo<'static> {
    vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::DONT_CARE)
}

/// 全幅 viewport，深度范围 `[0,1]`。
/// 坑：高度用正值，不要改成负高度翻转 Y——Y 已在 shader 里翻过，再翻画面会倒过来（见 gbuffer_pipeline 模块注释）。
///
/// # Safety
/// 见模块注释。
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

/// 全幅 viewport 加全幅 scissor。
///
/// # Safety
/// 见模块注释。
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

/// 绑图形管线，并从 set 0 起依次绑定 `sets`（无动态偏移）。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn bind_graphics(
    device: &Device,
    cb: vk::CommandBuffer,
    pipeline: &PipelineRef,
    sets: &[DescSet],
) {
    assert_set_sequence(pipeline.sets, sets);
    let raw: Vec<vk::DescriptorSet> = sets.iter().map(|s| s.raw()).collect();
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline.layout,
            0,
            &raw,
            &[],
        );
    }
}

/// 整块绑定顶点/索引 buffer（binding 0、偏移 0、U32 索引）；每个 draw 靠 vertexOffset/firstIndex 定位自己的数据。
///
/// # Safety
/// 见模块注释。
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
