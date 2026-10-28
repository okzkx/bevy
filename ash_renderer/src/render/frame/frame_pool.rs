//! 帧级资源：命令池/命令缓冲 + 每帧"image_available + fence"。
//!
//! 生命周期四层里的"帧级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：MAX_FRAMES_IN_FLIGHT 组
//! 轮转复用，CPU 最多领先 GPU 这么多帧。与 swapchain 解耦的是帧槽对象，各归其位：
//! - in_flight fence：GPU → CPU 重录闸门，按帧槽轮转；
//! - image_available：present engine → GPU 渲染，按帧槽轮转（复用安全前提：同槽两次
//!   acquire 隔 MAX_FRAMES_IN_FLIGHT 帧，且 fence 保证上一轮提交真的执行完了——
//!   提交失败路径一律 Tier② 退出，不留"signal 无人等"的残局）；
//! - render_finished（GPU → present engine）：**按 swapchain image 分配**，住在
//!   [`crate::render::Swapchain`]。同一 image 再次被 acquire 即证明其上一轮
//!   present 已被 present engine 消费完——这是帧 fence 给不了的保证（fence 只证提交
//!   完成，不证 present engine 的消费进度；依据 Khronos Vulkan Guide "Swapchain
//!   semaphore reuse"；同一队列上提交有序，下一次 signal 排在前一个 present 消费之后）。
//! - **深度附件按帧槽分配**：数量 = MAX_FRAMES_IN_FLIGHT，本体在 [`crate::render::core::depth_target`]。
//!   两份同步证据：① 在飞的两帧各用各的槽位深度，并发写
//!   不可能撞同一张图；② 同槽深度的两次使用之间隔着 `wait_for_slot` 的 fence
//!   等待（录制前 CPU 已确认上一轮提交完成）——不把"归帧级"当无冲突证明，冲突
//!   不存在是由 fence + 槽位隔离证出来的。尺寸随 swapchain 重建（[`Self::rebuild_depth`]，
//!   extent 不变即空手而归），销毁前自带 device_wait_idle 与 swapchain 重建同纪律。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::core::depth_target::DepthTarget;
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};


/// 一组在飞资源。image_available 桥接 present engine → GPU 渲染，in_flight 桥接
/// GPU → CPU（重录闸门）；present 方向的 render_finished 按 image 住在 Swapchain。
/// 深度附件按帧槽各一份（见模块注释的同步证据）。
pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    pub image_available: vk::Semaphore,
    pub in_flight: vk::Fence,
    /// 本槽位的深度附件（D32_SFLOAT，随 swapchain 尺寸重建）。
    pub depth: DepthTarget,
}

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
    /// 帧槽位游标：与 swapchain 的 image index 是两个独立循环（见模块注释）
    pub(super) current: usize,
    /// 深度附件的当前尺寸（与 swapchain.extent 同步，rebuild_depth 的幂等判据）。
    depth_extent: vk::Extent2D,
}

impl FramePool {
    /// 建帧资源：命令池/命令缓冲 + 双信号量/fence + 每槽一张深度附件（尺寸取
    /// swapchain 首建 extent，重建走 [`Self::rebuild_depth`]）。
    ///
    /// # Errors
    /// 命令池/命令缓冲/信号量/fence/深度附件创建失败。
    pub fn new(ctx: &Context, swapchain_extent: vk::Extent2D) -> Result<Self> {
        let command_pool = unsafe {
            ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 单线程录制 + 每帧整体重录，RESET_COMMAND_BUFFER 足够
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
        let mut depths = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            depths.push(DepthTarget::new(ctx, swapchain_extent)?);
        }
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
        let mut frames = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for (index, command_buffer) in command_buffers.into_iter().enumerate() {
            frames.push(Frame {
                command_buffer,
                image_available: guard.image_available[index],
                in_flight: guard.fences[index],
                depth: depths.swap_remove(0),
            });
        }
        info!(
            "帧资源就绪：{MAX_FRAMES_IN_FLIGHT} 组在飞（命令缓冲 + image_available + fence + 深度附件 {}x{}；render_finished 按 image 住在 Swapchain）",
            swapchain_extent.width, swapchain_extent.height,
        );
        let command_pool = std::mem::replace(&mut guard.command_pool, vk::CommandPool::null());
        guard.image_available.clear();
        guard.fences.clear();
        Ok(Self {
            device: ctx.device.clone(),
            command_pool,
            frames,
            current: 0,
            depth_extent: swapchain_extent,
        })
    }

    /// 当前帧槽位。与 `wait_for_slot`（等待+重置入口）之间不得 `advance`。
    pub fn current(&self) -> &Frame {
        &self.frames[self.current]
    }

    /// 当前帧槽位号（0/1）：set1 每帧 UBO 按它取（`BindlessTables::frame_set/frame_ubo`）。
    #[must_use]
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 第 `slot` 帧槽的深度 image 句柄（Hi-Z 归约 level 0 源视图的挂载目标，
    /// HizPyramid 建/重建时取）。
    #[must_use]
    pub fn depth_image(&self, slot: usize) -> vk::Image {
        self.frames[slot].depth.image
    }

    /// 第 `slot` 帧槽的深度视图句柄（G-buffer 采样集 b3 的来源）。
    #[must_use]
    pub fn depth_view(&self, slot: usize) -> vk::ImageView {
        self.frames[slot].depth.view
    }

    /// 轮转到下一组帧资源（提交完成后调）。
    pub fn advance(&mut self) {
        self.current = (self.current + 1) % self.frames.len();
    }

    /// 深度附件随 swapchain 尺寸重建（幂等：extent 没变就空手而归）。调用方是
    /// host 的 resize 闸门，紧跟成功的 `Swapchain::rebuild`；销毁旧深度前自带
    /// device_wait_idle——在飞帧可能还引用着旧深度，与 swapchain 重建同纪律。
    ///
    /// # Errors
    /// 等待排空或新深度附件创建失败。
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

    /// 等待全部帧槽的在飞提交完成（池迁移的"图形最后使用"等待）。fence
    /// 只证提交完成：全部槽位 fence 皆 signaled ⇒ 没有任何命令缓冲还在 GPU 上
    /// 执行 ⇒ 在飞 draw 对旧池的读取已全部收口。已完成的槽立即通过，正常路径
    /// 至多等 MAX_FRAMES_IN_FLIGHT-1 个在飞尾帧；等待不重置 fence，与
    /// `wait_for_slot` 的"只等不重置"纪律一致。
    ///
    /// # Errors
    /// fence 等待失败（设备丢失等，Tier② 语义由调用方分流）。
    pub fn wait_all_inflight(&self) -> Result<()> {
        let fences: Vec<vk::Fence> = self.frames.iter().map(|f| f.in_flight).collect();
        unsafe { self.device.wait_for_fences(&fences, true, u64::MAX)? };
        Ok(())
    }

    /// 帧首：等本槽位上一轮提交完成（GPU 侧 fence），重置命令缓冲。
    ///
    /// 这里**只等不重置 fence**。fence 的重置挪到
    /// [`Self::record_frame`] 提交前一刻——若在帧首重置，其后任何提前
    /// return（acquire OUT_OF_DATE / acquire 失败 / 重建失败）都会留下一个永远
    /// 无人 signal 的 unsignaled fence，下一帧 `wait_for_fences(u64::MAX)`
    /// 永久阻塞主线程和消息泵。
    pub fn wait_for_slot(&self) -> Result<()> {
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

impl Frame {
    /// 本槽位深度 image（进场屏障的目标）。
    pub(super) fn depth_image(&self) -> vk::Image {
        self.depth.image
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // 同步对象逐个销毁；命令池销毁连带释放其命令缓冲；深度附件随 Frame Drop
        //（DepthTarget 自拆 view/image/memory）
        for frame in self.frames.drain(..) {
            unsafe {
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_fence(frame.in_flight, None);
            }
            drop(frame.depth);
        }
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}
