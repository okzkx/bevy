//! 图像布局屏障的录制助手。用哪个阶段、访问、布局由调用方传入，屏障为什么这样设的说明写在调用处。
//!
//! 所有 `unsafe fn` 的前提与 ash 的 `cmd_*` 相同：`cb` 处于录制状态，传入的句柄合法且活到命令执行完。

use std::slice;

use ash::{vk, Device};

/// 一条图像布局屏障，只作用于第 0 层 mip、第 0 层 layer，不做队列族所有权转移。
/// 每条单独录成一次 `vkCmdPipelineBarrier`，阶段掩码就用本条的 src/dst。
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
    /// 丢弃旧内容、转成颜色附件准备写入。只等 `src_stage` 执行完，不需要让旧内容可见。
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

    /// 颜色附件写完，转成片元着色器可采样。
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

/// 录一条图像屏障。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn image_barrier(device: &Device, cb: vk::CommandBuffer, b: &ImageBarrier) {
    unsafe { image_barrier_with_memory(device, cb, b, &[]) };
}

/// 图像屏障和全局内存屏障合成一次 `vkCmdPipelineBarrier` 录出。
/// 坑：阶段掩码只取 `b` 的 src/dst，必须同时覆盖 `memory` 里的访问。
///
/// # Safety
/// 见模块注释。
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
