//! 帧槽：命令池/命令缓冲 + 每槽 image_available、in_flight fence、深度附件。
//!
//! `MAX_FRAMES_IN_FLIGHT` 组轮转复用，CPU 最多领先 GPU 这么多帧。帧槽与 swapchain image 是两个
//! 独立循环，重建 swapchain 时帧槽对象原地不动：
//! - in_flight fence：GPU → CPU，按帧槽轮转，CPU 等它才重录本槽；
//! - image_available：present engine → GPU，按帧槽轮转。复用安全：同槽两次 acquire 隔
//!   `MAX_FRAMES_IN_FLIGHT` 帧，且 fence 保证上一轮提交已执行完（提交失败一律报错退出，不会留下
//!   signal 了没人等的信号量）；
//! - render_finished：GPU → present engine，**按 swapchain image 分配**，住在
//!   [`crate::render::Swapchain`]。fence 只证明提交执行完，不证明 present engine 已消费完；
//!   同一 image 被再次 acquire 才证明它上一轮 present 已完成。
//! - 深度附件按帧槽各一份（本体见 [`crate::render::core::target::depth`]）。同步证据：在飞的两帧
//!   各用各的深度图；同槽两次使用之间隔着 `wait_for_slot` 的 fence。尺寸随 swapchain 重建
//!   （[`FramePool::rebuild_depth`]）。

use std::slice;

use ash::{vk, Device};
use bevy::ecs::error::BevyError;
use bevy::log::info;

use crate::common::error::Result;
use crate::common::syntax::ResultTierExt;
use crate::render::core::target::depth::DepthTarget;
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

/// 一组在飞资源（同步方向见模块注释）。
pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    pub image_available: vk::Semaphore,
    pub in_flight: vk::Fence,
    /// 本槽的深度附件（D32_SFLOAT，随 swapchain 尺寸重建）。
    pub depth: DepthTarget,
}

/// 建帧槽途中失败时回收已建的同步对象与命令池（命令缓冲随池释放）。
struct FramePoolBuildGuard<'a> {
    device: &'a Device,
    command_pool: vk::CommandPool,
    image_available: Vec<vk::Semaphore>,
    fences: Vec<vk::Fence>,
}

impl Drop for FramePoolBuildGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            for fence in self.fences.drain(..) {
                self.device.destroy_fence(fence, None);
            }
            for semaphore in self.image_available.drain(..) {
                self.device.destroy_semaphore(semaphore, None);
            }
            if self.command_pool != vk::CommandPool::null() {
                self.device.destroy_command_pool(self.command_pool, None);
            }
        }
    }
}

#[derive(bevy::prelude::Resource)]
pub struct FramePool {
    pub(super) device: Device,
    command_pool: vk::CommandPool,
    pub(super) frames: Vec<Frame>,
    /// 当前帧槽号，与 swapchain 的 image index 无关。
    pub(super) current: usize,
    /// 深度附件当前尺寸（rebuild_depth 据此判断是否需要重建）。
    depth_extent: vk::Extent2D,
}

impl FramePool {
    /// 建命令池/命令缓冲 + 每槽信号量/fence/深度附件（尺寸取 swapchain 首建 extent）。
    ///
    /// # Errors
    /// 命令池/命令缓冲/信号量/fence/深度附件创建失败。
    pub fn new(ctx: &Context, swapchain_extent: vk::Extent2D) -> Result<Self> {
        let command_pool = unsafe {
            ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 单线程录制、每帧整体重录，按缓冲单独重置即可
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family_index),
                None,
            )
        }?;
        let mut guard = FramePoolBuildGuard {
            device: &ctx.device,
            command_pool,
            image_available: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
            fences: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
        };
        let command_buffers = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
            )
        }?;
        let depths = (0..MAX_FRAMES_IN_FLIGHT)
            .map(|_| DepthTarget::new(ctx, swapchain_extent))
            .collect::<Result<Vec<_>>>()?;
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            guard.image_available.push(unsafe {
                ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
            guard.fences.push(unsafe {
                ctx.device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }?);
        }
        // 全部建成：所有权从 guard 移交给帧槽
        let frames = command_buffers
            .into_iter()
            .zip(depths)
            .zip(guard.image_available.drain(..).zip(guard.fences.drain(..)))
            .map(|((command_buffer, depth), (image_available, in_flight))| Frame {
                command_buffer,
                image_available,
                in_flight,
                depth,
            })
            .collect();
        let command_pool = std::mem::replace(&mut guard.command_pool, vk::CommandPool::null());
        info!(
            "帧资源就绪：{MAX_FRAMES_IN_FLIGHT} 组在飞（命令缓冲 + image_available + fence + 深度附件 {}x{}；render_finished 按 image 住在 Swapchain）",
            swapchain_extent.width, swapchain_extent.height,
        );
        Ok(Self {
            device: ctx.device.clone(),
            command_pool,
            frames,
            current: 0,
            depth_extent: swapchain_extent,
        })
    }

    /// 当前帧槽。与 `wait_for_slot` 之间不得 `advance`。
    pub fn current(&self) -> &Frame {
        &self.frames[self.current]
    }

    /// 当前帧槽号。
    #[must_use]
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 第 `slot` 帧槽的深度 image（Hi-Z 归约的源）。
    #[must_use]
    pub fn depth_image(&self, slot: usize) -> vk::Image {
        self.frames[slot].depth.image
    }

    /// 第 `slot` 帧槽的深度视图（G-buffer 采样集 b3）。
    #[must_use]
    pub fn depth_view(&self, slot: usize) -> vk::ImageView {
        self.frames[slot].depth.view
    }

    /// 轮转到下一帧槽（提交后调用）。
    pub fn advance(&mut self) {
        self.current = (self.current + 1) % self.frames.len();
    }

    /// 深度附件随 swapchain 尺寸重建，extent 没变则什么都不做。由 host 在
    /// `Swapchain::rebuild` 成功后调用；销毁旧深度前先 device_wait_idle（在飞帧可能还在用）。
    ///
    /// # Errors
    /// 等待或新深度附件创建失败。
    pub fn rebuild_depth(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if self.depth_extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        for frame in &mut self.frames {
            frame.depth = DepthTarget::new(ctx, extent)?;
        }
        self.depth_extent = extent;
        info!(
            "深度附件随 swapchain 重建: {}x{} × {} 帧槽",
            extent.width,
            extent.height,
            self.frames.len(),
        );
        Ok(())
    }

    /// 等全部帧槽的在飞提交完成：全部 fence signaled ⇒ 没有命令缓冲还在 GPU 上执行。回收旧资源
    /// 前用它确认 GPU 不再读。只等不重置 fence（同 `wait_for_slot`）。
    ///
    /// # Errors
    /// fence 等待失败（如设备丢失）。
    pub fn wait_all_inflight(&self) -> Result<()> {
        let fences: Vec<vk::Fence> = self.frames.iter().map(|f| f.in_flight).collect();
        unsafe { self.device.wait_for_fences(&fences, true, u64::MAX)? };
        Ok(())
    }

    /// 帧首：等本槽上一轮提交完成，重置命令缓冲。
    ///
    /// 坑：这里只等不重置 fence，重置挪到 [`Self::record_frame`] 提交前一刻。若在帧首重置，
    /// 之后任何提前 return（acquire OUT_OF_DATE / acquire 失败 / 重建失败）都会留下一个永远
    /// 没人 signal 的 fence，下一帧 `wait_for_fences(u64::MAX)` 永久卡死主线程和消息泵。
    ///
    /// # Errors
    /// fence 等待或命令缓冲重置失败。
    /// 等本帧槽上一轮提交完成 + 重置命令缓冲（fence 的重置在提交前一刻）。
    /// 失败 = 设备丢失/内存枯竭类，继续只会撞上未知的 fence 状态——按致命错误冒泡，
    /// 不得 warn 后继续。
    pub fn wait_for_slot(&self) -> Result<(), BevyError> {
        self.wait_fenced()
            .or_fatal("帧槽等待/重置失败，渲染链无法继续")
    }

    fn wait_fenced(&self) -> Result<()> {
        let frame = &self.frames[self.current];
        unsafe {
            self.device
                .wait_for_fences(slice::from_ref(&frame.in_flight), true, u64::MAX)?;
            self.device.reset_command_buffer(
                frame.command_buffer,
                vk::CommandBufferResetFlags::empty(),
            )?;
        }
        Ok(())
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。命令池连带释放命令缓冲；
        // 深度附件随 Frame Drop。
        for frame in self.frames.drain(..) {
            unsafe {
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_fence(frame.in_flight, None);
            }
        }
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}
