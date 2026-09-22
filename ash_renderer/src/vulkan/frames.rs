//! 帧级资源（施工④从 vulkan.rs 拆出）：命令池/命令缓冲 + 每帧"双信号量 + fence"。
//!
//! 生命周期四层里的"帧级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：MAX_FRAMES_IN_FLIGHT 组
//! 轮转复用，CPU 最多领先 GPU 这么多帧；与 swapchain 解耦——同步对象不挂在任何一张
//! swapchain image 上，swapchain 重建时原地不动。复用安全的前提：同组资源两次使用
//! 至少隔 `MAX_FRAMES_IN_FLIGHT` 帧，且 fence 保证上一轮提交已全部执行完。
//!
//! 清屏本身走 Vulkan 1.3 动态渲染（feature 在 `super::context` 设备创建时已开）：
//! 无 RenderPass/Framebuffer/管线，`loadOp=CLEAR` 就是清屏——本步要的是"颜色对了 +
//! 帧在动"，管线是 step3 的事。两个布局屏障是每帧的进出场纪律：
//! acquire 后的 image 布局不确定 → COLOR_ATTACHMENT_OPTIMAL → PRESENT_SRC_KHR。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::error::VulkanError;
use crate::vulkan::Context;

/// CPU 领先 GPU 的最大帧数：第 N 帧要等第 N-2 帧的组资源空出来才能重录。
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// 一组在飞资源。三个对象各管一段：image_available 桥接 present engine → GPU 渲染，
/// render_finished 桥接 GPU → present engine，in_flight 桥接 GPU → CPU（重录闸门）。
pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    pub image_available: vk::Semaphore,
    pub render_finished: vk::Semaphore,
    pub in_flight: vk::Fence,
}

#[derive(bevy::prelude::Resource)]
pub struct FramePool {
    device: Device,
    command_pool: vk::CommandPool,
    frames: Vec<Frame>,
    /// 帧槽位游标：与 swapchain 的 image index 是两个独立循环（见模块注释）
    current: usize,
}

impl FramePool {
    pub fn new(ctx: &Context) -> Result<Self, VulkanError> {
        let command_pool = unsafe {
            ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 单线程录制 + 每帧整体重录，RESET_COMMAND_BUFFER 足够
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family_index),
                None,
            )
        }?;
        let command_buffers = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
            )
        }?;
        let frames = command_buffers
            .iter()
            .map(|&command_buffer| {
                Ok(Frame {
                    command_buffer,
                    image_available: unsafe {
                        ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    }?,
                    render_finished: unsafe {
                        ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    }?,
                    // 初值 SIGNALED：第一帧"等上一轮完成"立即通过
                    in_flight: unsafe {
                        ctx.device.create_fence(
                            &vk::FenceCreateInfo::default()
                                .flags(vk::FenceCreateFlags::SIGNALED),
                            None,
                        )
                    }?,
                })
            })
            .collect::<Result<Vec<_>, VulkanError>>()?;
        info!("帧资源就绪：{MAX_FRAMES_IN_FLIGHT} 组在飞（命令缓冲 + 双信号量 + fence）");
        Ok(Self {
            device: ctx.device.clone(),
            command_pool,
            frames,
            current: 0,
        })
    }

    /// 当前帧槽位。与 `wait_and_reset` 之间不得 `advance`。
    pub fn current(&self) -> &Frame {
        &self.frames[self.current]
    }

    /// 轮转到下一组帧资源（提交完成后调）。
    pub fn advance(&mut self) {
        self.current = (self.current + 1) % self.frames.len();
    }

    /// 等本槽位上一轮提交完成（GPU 侧 fence），然后重置 fence 与命令缓冲。
    pub fn wait_and_reset(&self) -> Result<(), VulkanError> {
        let frame = &self.frames[self.current];
        unsafe {
            self.device
                .wait_for_fences(slice::from_ref(&frame.in_flight), true, u64::MAX)?;
            self.device
                .reset_fences(slice::from_ref(&frame.in_flight))?;
            self.device.reset_command_buffer(
                frame.command_buffer,
                vk::CommandBufferResetFlags::empty(),
            )?;
        }
        Ok(())
    }

    /// 录制并提交一帧清屏：进场屏障 → dynamic rendering（loadOp 上色）→ 出场屏障 →
    /// queue_submit。提交等 acquire 的 image_available，完成时发 render_finished + fence。
    pub fn record_clear_and_submit(
        &self,
        ctx: &Context,
        image: vk::Image,
        view: vk::ImageView,
        extent: vk::Extent2D,
        clear: [f32; 4],
    ) -> Result<(), VulkanError> {
        let frame = &self.frames[self.current];
        unsafe {
            self.device.begin_command_buffer(
                frame.command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // 进场：旧布局不关心（loadOp 全清），等 TOP_OF_PIPE 后可改写；
            // 写访问到 COLOR_ATTACHMENT_OUTPUT 阶段才发生，屏障不需要更早可见
            let to_color_attachment = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            self.device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_color_attachment],
            );

            // 动态渲染：附件内联声明，begin 即按 loadOp 清屏，end 即按 storeOp 落盘
            let color_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: clear },
                });
            self.device.cmd_begin_rendering(
                frame.command_buffer,
                &vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    })
                    .layer_count(1)
                    .color_attachments(slice::from_ref(&color_attachment)),
            );
            self.device.cmd_end_rendering(frame.command_buffer);

            // 出场：写完到 present 引擎接管之间必须收口；PRESENT_SRC 是 present 专用布局
            let to_present_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            self.device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present_src],
            );
            self.device.end_command_buffer(frame.command_buffer)?;

            // 等待点语义：COLOR_ATTACHMENT_OUTPUT 起才真正需要 image 已到手——
            // 之前的顶点处理等可以和 acquire 重叠（清屏阶段用不上，但这是以后画东西的正确姿势）
            let wait_dst_stage = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            self.device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(slice::from_ref(&frame.image_available))
                    .wait_dst_stage_mask(&wait_dst_stage)
                    .command_buffers(slice::from_ref(&frame.command_buffer))
                    .signal_semaphores(slice::from_ref(&frame.render_finished))],
                frame.in_flight,
            )?;
        }
        Ok(())
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // 同步对象逐个销毁；命令池销毁连带释放其命令缓冲
        for frame in self.frames.drain(..) {
            unsafe {
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_semaphore(frame.render_finished, None);
                self.device.destroy_fence(frame.in_flight, None);
            }
        }
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}
