//! 交换链：窗口尺寸一变就整体重建，images、views、格式、尺寸全换。
//!
//! 帧槽对象（fence、image_available、命令缓冲）跨重建复用，不归这里管；
//! render_finished 信号量按 image 分配，挂在本结构下随重建一起换（原因见 `frame/frame_pool.rs` 模块注释）。

use ash::{khr::swapchain, vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::Context;

/// acquire 成功的两种结局（OUT_OF_DATE 走 `Err`）。`Suboptimal` 也拿到了图：照常画完这帧，之后再重建。
#[derive(Debug, Clone, Copy)]
pub enum AcquireOutcome {
    Ready(u32),
    Suboptimal(u32),
}

/// present 成功的两种结局。`Suboptimal` 表示这帧已交出，之后需要重建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Done,
    Suboptimal,
}

/// `Swapchain::new` 中途失败时的回滚护栏：析构时销毁已建成的对象；成功后把句柄取走，析构即空操作。
struct SwapchainBuildGuard {
    fns: swapchain::Device,
    device: Device,
    swapchain: vk::SwapchainKHR,
    views: Vec<vk::ImageView>,
    semaphores: Vec<vk::Semaphore>,
}

impl Drop for SwapchainBuildGuard {
    fn drop(&mut self) {
        unsafe {
            for semaphore in self.semaphores.drain(..) {
                self.device.destroy_semaphore(semaphore, None);
            }
            for view in self.views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            if self.swapchain != vk::SwapchainKHR::null() {
                self.fns.destroy_swapchain(self.swapchain, None);
            }
        }
    }
}

#[derive(bevy::prelude::Resource)]
pub struct Swapchain {
    fns: swapchain::Device,
    /// 销毁 image view 和信号量要用
    device: Device,
    pub swapchain: vk::SwapchainKHR,
    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,
    /// present 等待的信号量，每张 image 一个。坑：按 acquire 返回的 image index 取用，不是按帧槽轮转。
    pub render_finished: Vec<vk::Semaphore>,
    /// swapchain image 本身的格式，present 直接读它。
    pub format: vk::Format,
    /// 渲染时写入用的 view 格式：通常是 `format` 的 SRGB 版本，写入时硬件自动做 sRGB 编码；
    /// 不支持时等于 `format`。
    pub view_format: vk::Format,
    pub extent: vk::Extent2D,
}

/// swapchain 输出格式常量：BGRA8-UNORM 底板的 SRGB view 别名（3.4 输出编码定案）。
/// [`Swapchain::new`] 的闸门保证 view_format 恒等于它——管线配方按此常量声明附件。
pub const SWAPCHAIN_COLOR_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;

impl Swapchain {
    /// 设备支持把 `srgb` 当颜色附件才返回它，否则 `None`。
    fn srgb_alias_if_supported(ctx: &Context, srgb: vk::Format) -> Option<vk::Format> {
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, srgb)
        }
        .optimal_tiling_features;
        features
            .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
            .then_some(srgb)
    }

    /// 按窗口当前尺寸建交换链。偏好 BGRA8_UNORM 格式、MAILBOX 呈现模式（不支持时退回 FIFO）。
    pub fn new(ctx: &Context) -> Result<Self> {
        let fns = swapchain::Device::new(&ctx.instance, &ctx.device);

        // 每次重建都重查：current_extent 是驱动认定的窗口尺寸，以它为准
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
        // 输出编码定案（3.4）：BGRA8-UNORM 底板 + SRGB view 别名。格式是管线配方
        // 的常量（[`SWAPCHAIN_COLOR_FORMAT`]），不支持的设备显式报错退出——不静默
        // 退回（退回 = 受光几何整体偏暗的隐性降级）。
        let surface_format = formats
            .iter()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .copied()
            .ok_or_else(|| {
                VulkanError::Init(format!(
                    "surface 不支持 BGRA8-UNORM（候选：{:?}）——swapchain 格式是管线配方的常量，不静默降级",
                    formats.iter().map(|f| f.format).collect::<Vec<_>>()
                ))
            })?;
        // image 用 UNORM 格式，渲染时套同一张图的 SRGB view 写入，写入时自动做 sRGB 编码，present 读 image 原样上屏。
        // 合法需要三样：context.rs 启用的 mutable_format 扩展、下面的格式清单和 MUTABLE_FORMAT 旗标、SRGB 格式支持当颜色附件。
        let view_format = Self::srgb_alias_if_supported(ctx, vk::Format::B8G8R8A8_SRGB).ok_or_else(|| {
            VulkanError::Init("BGRA8-UNORM 无 SRGB view 别名（扩展/格式支持不足）——输出编码定案无法生效，显式退出".to_owned())
        })?;
        info!("输出编码：UNORM 底板 + SRGB view 别名（ROP 写出自动编码，着色器全程线性）");
        // 坑：格式清单必须包含 swapchain 自身格式（VUID-VkSwapchainCreateInfoKHR-pNext-07781）；
        // format_list 和 format_list_info 必须活到 create_swapchain 返回（pNext 只存指针）
        let format_list: Vec<vk::Format> = vec![surface_format.format, view_format];
        let mut format_list_info =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list);
        if caps.current_extent.width == u32::MAX {
            return Err(VulkanError::Init("current_extent 未定义（窗口尚未定型），不做显式尺寸回退".into()));
        }
        // MAILBOX 优先：新帧直接替换还没上屏的帧，acquire 不排队。
        // 坑：FIFO 的 acquire 在显示队列满时会阻塞，造成最小化死锁和拖拽窗口冻结；FIFO 只作必有的兜底
        let present_modes = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_present_modes(ctx.physical_device, ctx.surface)
        }?;
        let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        // MAILBOX 至少要 3 张 image：一张在显示、一张排队待替换、一张给下一帧画；同时不超过驱动上限
        let mut image_count = caps.min_image_count + 1;
        if present_mode == vk::PresentModeKHR::MAILBOX && image_count < 3 {
            image_count = 3;
        }
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
                    .present_mode(present_mode)
                    .clipped(true)
                    // 坑：有 SRGB view 时必须带这个旗标，否则验证层报 VUID-VkSwapchainCreateInfoKHR-flags-01977
                    .flags(if view_format == SWAPCHAIN_COLOR_FORMAT {
                        vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT
                    } else {
                        vk::SwapchainCreateFlagsKHR::empty()
                    })
                    .push_next(&mut format_list_info),
                None,
            )
        }?;
        let mut guard = SwapchainBuildGuard {
            fns: fns.clone(),
            device: ctx.device.clone(),
            swapchain,
            views: Vec::new(),
            semaphores: Vec::new(),
        };
        let images = unsafe { fns.get_swapchain_images(swapchain) }?;
        guard.views.reserve(images.len());
        for &image in &images {
            guard.views.push(unsafe {
                ctx.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(view_format)
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .level_count(1)
                                .layer_count(1),
                        ),
                    None,
                )
            }?);
        }

        guard.semaphores.reserve(images.len());
        for _ in &images {
            guard.semaphores.push(unsafe {
                ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }

        info!(
            "swapchain 就绪: {}x{}，{} images，底板 {:?} / 渲染 view {:?}，present={:?}，render_finished 按 image 配 {} 个",
            caps.current_extent.width,
            caps.current_extent.height,
            images.len(),
            surface_format.format,
            view_format,
            present_mode,
            guard.semaphores.len(),
        );

        let views = std::mem::take(&mut guard.views);
        let render_finished = std::mem::take(&mut guard.semaphores);
        let swapchain = std::mem::replace(&mut guard.swapchain, vk::SwapchainKHR::null());
        Ok(Self {
            fns,
            device: ctx.device.clone(),
            swapchain,
            images,
            views,
            render_finished,
            format: surface_format.format,
            view_format,
            extent: caps.current_extent,
        })
    }

    /// 窗口尺寸变化后重建。窗口最小化（尺寸为 0）或尺寸没变时什么都不做。
    ///
    /// 坑：一次真重建约 60ms，调用方必须去抖（`driver/host/frame.rs` 的 `ResizeGate`），不要拖拽中每步都调。
    pub fn rebuild(&mut self, ctx: &Context) -> Result<()> {
        let caps = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_capabilities(ctx.physical_device, ctx.surface)
        }?;
        // 0 尺寸不能建 swapchain：保留旧的，窗口恢复后会再收到 resize
        if caps.current_extent.width == 0 || caps.current_extent.height == 0 {
            info!("窗口最小化（extent=0），swapchain 暂不重建");
            return Ok(());
        }
        // 坑：上次重建失败会留下空壳（句柄 null、extent 是旧值），尺寸相等也必须重建，否则之后每帧都拿 null 去 acquire
        if caps.current_extent == self.extent && self.swapchain != vk::SwapchainKHR::null() {
            return Ok(());
        }
        // 在飞帧还在用旧 image，先等 GPU 全部做完再拆。
        // 坑：wait_idle 只保证队列做完，不保证显示端已用完 render_finished；但 Win32 同一窗口只能挂一个
        // swapchain，只能先拆旧再建新，没法把旧的留到以后再回收
        unsafe { ctx.device.device_wait_idle() }?;
        // 坑：不能直接写 `*self = Self::new(ctx)?`——右边先求值，会变成先建新再拆旧
        self.destroy();
        *self = Self::new(ctx)?; // 失败时 self 是空壳，下次调用不论尺寸都会重试
        Ok(())
    }

    /// 销毁 views、信号量和 swapchain。可重复调用：拆完句柄清空，再调是空操作。
    /// 不等 GPU，调用方要先保证没有在用的帧（见 `rebuild`）。
    fn destroy(&mut self) {
        for view in self.views.drain(..) {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        for sem in self.render_finished.drain(..) {
            unsafe { self.device.destroy_semaphore(sem, None) };
        }
        if self.swapchain != vk::SwapchainKHR::null() {
            unsafe { self.fns.destroy_swapchain(self.swapchain, None) };
            self.swapchain = vk::SwapchainKHR::null();
        }
    }

    /// 取下一张可画的 image；这张图真正可写时 `semaphore` 被置位。
    /// OUT_OF_DATE 返回 [`VulkanError::SwapchainOutOfDate`]。
    pub fn acquire(&self, semaphore: vk::Semaphore) -> Result<AcquireOutcome> {
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

    /// 等 `wait_semaphore`（该 image 的 render_finished）后把 image 交给显示。
    /// OUT_OF_DATE 返回 [`VulkanError::SwapchainOutOfDate`]。
    pub fn present(
        &self,
        queue: vk::Queue,
        wait_semaphore: vk::Semaphore,
        index: u32,
    ) -> Result<PresentOutcome> {
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
        // 依赖 Device 还活着：正常退出由 teardown_vulkan 保证先拆本结构、最后拆 Context；
        // panic 时拆除顺序不定，不作保证
        self.destroy();
    }
}
