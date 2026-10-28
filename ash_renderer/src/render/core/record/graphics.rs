//! 图形管线的录制助手：绑定管线与描述符集、顶点/索引 buffer、viewport/scissor。
//! 所有 `unsafe fn` 的前提与 ash 的 `cmd_*` 相同：`cb` 处于录制状态，传入的句柄合法且
//! 活到命令执行完。

use ash::{vk, Device};

use crate::render::core::binding::set_layout::{assert_set_sequence, DescSet};
use crate::render::core::pipeline::graphics::PipelineRef;

/// 覆盖整个 `extent` 的矩形，用作 renderArea 或 scissor。
pub(crate) fn full_rect(extent: vk::Extent2D) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }
}

/// 全幅 viewport，深度范围 [0,1]。
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
        device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.layout, 0, &raw, &[]);
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
