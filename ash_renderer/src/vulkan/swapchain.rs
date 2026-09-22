//! Swapchain（施工④从 vulkan.rs 拆出）：resize 级生命周期。
//!
//! 生命周期四层里的"resize 级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：窗口尺寸一变整体重建，
//! images/views/format/extent 全换；而 fence/信号量/命令缓冲不挂在任何一张 swapchain
//! image 上（在 `crate::vulkan::frames`），跨重建复用——这正是拆分点：重建 swapchain 时
//! 同步对象原地不动。
//!
//! `pre_transform`/`composite_alpha`/`present_mode` 等选型与创建链（施工③）一致：
//! FIFO 恒可用、BGRA8_UNORM 优先、EXCLUSIVE 共享、clipped。

use ash::{khr::swapchain, vk, Device};
use bevy::log::info;

use crate::error::VulkanError;
use crate::vulkan::Context;

/// acquire 的三种结局。SUBOPTIMAL 拿得到图但下次要重建——照常渲染完这帧再重建。
#[derive(Debug, Clone, Copy)]
pub enum AcquireOutcome {
    Ready(u32),
    Suboptimal(u32),
}

/// present 的结局：SUBOPTIMAL 同样是"这张已交出，重建下帧再说"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Done,
    Suboptimal,
}

/// swapchain + 每张 image 的 view。重建 = wait_idle 后整体换新（`rebuild`）。
#[derive(bevy::prelude::Resource)]
pub struct Swapchain {
    fns: swapchain::Device,
    /// image view 的创建/销毁在 Device 上（core 1.0），自持一份供 Drop 用
    device: Device,
    pub swapchain: vk::SwapchainKHR,
    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
}

impl Swapchain {
    pub fn new(ctx: &Context) -> Result<Self, VulkanError> {
        let fns = swapchain::Device::new(&ctx.instance, &ctx.device);

        // 每次重建都重查 caps：current_extent 是驱动认定的窗口尺寸，唯一的权威来源
        let caps = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_capabilities(ctx.physical_device, ctx.surface)
        }?;
        let formats = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_formats(ctx.physical_device, ctx.surface)
        }?;
        if formats.is_empty() {
            return Err(VulkanError::Init("surface 无可用格式".into()));
        }
        let surface_format = formats
            .iter()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .copied()
            .unwrap_or(formats[0]);
        if caps.current_extent.width == u32::MAX {
            return Err(VulkanError::Init("current_extent 未定义（窗口尚未定型），本步不做显式尺寸回退".into()));
        }
        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 && image_count > caps.max_image_count {
            image_count = caps.max_image_count;
        }

        let swapchain = unsafe {
            fns.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(ctx.surface)
                    .min_image_count(image_count)
                    .image_format(surface_format.format)
                    .image_color_space(surface_format.color_space)
                    .image_extent(caps.current_extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                    .present_mode(vk::PresentModeKHR::FIFO)
                    .clipped(true),
                None,
            )
        }?;
        // 建好之后取 images 若失败，swapchain 必须就地销毁——否则它一直占着 hwnd，
        // 下一次重建必报 ERROR_NATIVE_WINDOW_IN_USE_KHR
        let images = match unsafe { fns.get_swapchain_images(swapchain) } {
            Ok(images) => images,
            Err(e) => {
                unsafe { fns.destroy_swapchain(swapchain, None) };
                return Err(e.into());
            }
        };

        // view：后续所有渲染（含清屏）吃 view 不吃裸 image；生命周期与 swapchain 绑定
        let views = images
            .iter()
            .map(|&image| {
                unsafe {
                    ctx.device.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(surface_format.format)
                            .subresource_range(
                                vk::ImageSubresourceRange::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .level_count(1)
                                    .layer_count(1),
                            ),
                        None,
                    )
                }
                .map_err(VulkanError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;

        info!(
            "swapchain 就绪: {}x{}，{} images，{:?}",
            caps.current_extent.width,
            caps.current_extent.height,
            images.len(),
            surface_format.format,
        );

        Ok(Self {
            fns,
            device: ctx.device.clone(),
            swapchain,
            images,
            views,
            format: surface_format.format,
            extent: caps.current_extent,
        })
    }

    /// resize 后重建。两个幂等出口：
    /// - 最小化时 current_extent=0，建 0 尺寸 swapchain 非法——保留旧的，恢复后靠下一次 resize 消息重建；
    /// - 尺寸没变就不动。
    ///
    /// 单次真重建 ≈ 60ms（wait_idle+destroy+create，2026-09-22 实测）——调用方必须
    /// 去抖（host::draw_frame 的 `ResizeGate`），不要在拖拽的每个尺寸步进上调用。
    pub fn rebuild(&mut self, ctx: &Context) -> Result<(), VulkanError> {
        let caps = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_capabilities(ctx.physical_device, ctx.surface)
        }?;
        if caps.current_extent.width == 0 || caps.current_extent.height == 0 {
            info!("窗口最小化（extent=0），swapchain 暂不重建");
            return Ok(());
        }
        if caps.current_extent == self.extent {
            return Ok(());
        }
        // 在飞帧还在引用旧 image，必须先等全部完成再拆
        unsafe { ctx.device.device_wait_idle() }?;
        // Win32 的 WSI 限制：同一个 hwnd 同时只能挂一个 swapchain——
        // 必须先拆旧再建新（`*self = Self::new(ctx)` 的 RHS 先求值，是"先建后拆"，必炸）
        self.destroy();
        *self = Self::new(ctx)?; // 失败时 self 已是空壳（句柄为 null），下一帧 resize 消息进来重试
        Ok(())
    }

    /// 销毁 views + swapchain（幂等：句柄清 null 后重复调用是空操作）。
    fn destroy(&mut self) {
        for view in self.views.drain(..) {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        if self.swapchain != vk::SwapchainKHR::null() {
            unsafe { self.fns.destroy_swapchain(self.swapchain, None) };
            self.swapchain = vk::SwapchainKHR::null();
        }
    }

    /// 取下一张可绘 image（信号量在 present engine 拿到图时置位）。
    /// `ERROR_OUT_OF_DATE_KHR` 折叠成 [`VulkanError::SwapchainOutOfDate`]。
    pub fn acquire(&self, semaphore: vk::Semaphore) -> Result<AcquireOutcome, VulkanError> {
        match unsafe {
            self.fns
                .acquire_next_image(self.swapchain, u64::MAX, semaphore, vk::Fence::null())
        } {
            Ok((index, suboptimal)) => Ok(if suboptimal {
                AcquireOutcome::Suboptimal(index)
            } else {
                AcquireOutcome::Ready(index)
            }),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Err(VulkanError::SwapchainOutOfDate),
            Err(e) => Err(e.into()),
        }
    }

    /// 呈现：等渲染提交的 render_finished 信号量。
    pub fn present(
        &self,
        queue: vk::Queue,
        wait_semaphore: vk::Semaphore,
        index: u32,
    ) -> Result<PresentOutcome, VulkanError> {
        let wait = [wait_semaphore];
        let swapchains = [self.swapchain];
        let indices = [index];
        match unsafe {
            self.fns.queue_present(
                queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&wait)
                    .swapchains(&swapchains)
                    .image_indices(&indices),
            )
        } {
            Ok(suboptimal) => Ok(if suboptimal {
                PresentOutcome::Suboptimal
            } else {
                PresentOutcome::Done
            }),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Err(VulkanError::SwapchainOutOfDate),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // view 先于 swapchain。Device/Instance 的存活由拆除顺序保证：
        // 正常退出走 main.rs teardown（FramePool → 本结构 → Context）；
        // panic 时 Resource drop 顺序不定，进程本就注定终止。
        self.destroy();
    }
}
