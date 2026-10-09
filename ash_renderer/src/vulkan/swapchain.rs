//! Swapchain：resize 级生命周期。
//!
//! 生命周期四层里的"resize 级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：窗口尺寸一变整体重建，
//! images/views/format/extent 全换。帧槽对象（fence / image_available / 命令缓冲）跨重建
//! 复用；而 render_finished（present-wait 信号量）D3 修复后**按 image 分配**、挂在本结构
//! 下随重建换血——同一 image 再次被 acquire 即证明上一轮 present 已被消费完，这是
//! 唯一有依据的复用闸门，帧槽轮转给不了这个保证（详见 frames.rs 模块注释）。
//!
//! `pre_transform`/`composite_alpha`/`present_mode` 等选型与创建链（施工③）一致：
//! MAILBOX 优先（FIFO 兜底）、BGRA8_UNORM 优先、EXCLUSIVE 共享、clipped。
//!
//! 3.4 输出编码定案：image 保持 UNORM 底板，view 套 SRGB 别名当渲染附件（needs
//! `VK_KHR_swapchain_mutable_format` + VkImageFormatListCreateInfo，见 `new` 内定案
//! 注释）——本结构新增 `view_format` 字段承载"底板 vs 渲染 view"两个格式。

use ash::{khr::swapchain, vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
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
    /// D3：按 image index 分配的 present-wait 信号量（render_finished）。
    /// host.rs 按 acquire 返回的 index 取用，**不是**按帧槽轮转；生命周期与
    /// swapchain 绑定，重建时随旧 image 集合一起销毁。
    pub render_finished: Vec<vk::Semaphore>,
    /// swapchain 底板格式（present engine 的母语；D3 定案的 present 路径不碰 view）。
    pub format: vk::Format,
    /// 渲染附件 view 的格式：底板 UNORM 时的 SRGB 别名（输出编码定案，3.4）——
    /// 写入时 ROP 自动编码；别名不可用时 == `format`（缺位形态，重建日志有 warn）。
    pub view_format: vk::Format,
    pub extent: vk::Extent2D,
}

impl Swapchain {
    pub fn new(ctx: &Context) -> Result<Self> {
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
        // 输出编码定案（3.4，与 context.rs 的 swapchain_mutable_format 扩展配套）：
        // swapchain image 保持 UNORM 底板，view 用 SRGB 别名当渲染附件——写入时由
        // ROP 自动做线性→sRGB 编码（blend 在编码前，仍是线性域），present 直接读
        // image 本体不经 view，编码结果原样上屏。别名合法性三件套：
        // ① 设备扩展（context.rs 已查+启用）；② 创建时挂 VkImageFormatListCreateInfo
        // 声明双格式（swapchain image 由此按 MUTABLE_FORMAT 形态创建）；③ SRGB 变体
        // 的 color attachment 支持查过才用（支持与启用分开），不满足回退 UNORM 直出
        // 并响亮记日志——输出编码缺位会在受光中间调上显形（偏暗），不能静默。
        let srgb_alias = match surface_format.format {
            vk::Format::B8G8R8A8_UNORM => {
                let features = unsafe {
                    ctx.instance
                        .get_physical_device_format_properties(ctx.physical_device, vk::Format::B8G8R8A8_SRGB)
                }
                .optimal_tiling_features;
                if features.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT) {
                    Some(vk::Format::B8G8R8A8_SRGB)
                } else {
                    None
                }
            }
            _ => None,
        };
        let view_format = srgb_alias.unwrap_or(surface_format.format);
        if srgb_alias.is_some() {
            info!("输出编码：UNORM 底板 + SRGB view 别名（ROP 写出自动编码，着色器全程线性）");
        } else {
            bevy::log::warn!("输出编码缺位：swapchain {:?} 无 SRGB view 别名（扩展/格式支持不足），受光几何中间调将偏暗——3.4 输出编码定案未生效", surface_format.format);
        }
        // 格式清单必须含 swapchain 自身格式（VUID-VkSwapchainCreateInfoKHR-pNext-07781
        // 族的声明要求：viewFormats 覆盖创建格式）；pNext 结构须活到 create 返回
        let format_list: Vec<vk::Format> = match srgb_alias {
            Some(srgb) => vec![surface_format.format, srgb],
            None => vec![surface_format.format],
        };
        let mut format_list_info =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list);
        if caps.current_extent.width == u32::MAX {
            return Err(VulkanError::Init("current_extent 未定义（窗口尚未定型），本步不做显式尺寸回退".into()));
        }
        // present mode：MAILBOX 优先（frenderer 同款选型，2026-09-22 定案）。FIFO 的
        // acquire 会在显示队列满/表面失配时阻塞（最小化死锁、拖拽冻结的根源）；
        // MAILBOX 的 present 直接替换未上屏的帧、acquire 永不排队——拖拽中帧循环
        // 持续流动。MAILBOX 不保证恒可用，FIFO 兜底（规范唯一保证）。
        let present_modes = unsafe {
            ctx.surface_fns
                .get_physical_device_surface_present_modes(ctx.physical_device, ctx.surface)
        }?;
        let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        // MAILBOX 的语义要 ≥3 张 image 才成立（呈现中 + 被替换的入队帧 + 可 acquire），
        // min+1 通常恰为 3；不足则加到 3（仍尊重 max_image_count）。
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
                    // 双格式清单必须配本旗标：swapchain image 由此按 MUTABLE_FORMAT
                    // 形态创建，SRGB view 别名才合法（VUID-VkSwapchainCreateInfoKHR-
                    // flags-01977；首跑验证层实抓）
                    .flags(if srgb_alias.is_some() {
                        vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT
                    } else {
                        vk::SwapchainCreateFlagsKHR::empty()
                    })
                    .push_next(&mut format_list_info),
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

        // view：渲染（含清屏）吃 view 不吃裸 image；格式用 SRGB 别名（输出编码，
        // 见上方定案注释）——生命周期与 swapchain 绑定
        let views = images
            .iter()
            .map(|&image| {
                unsafe {
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
                }
                .map_err(VulkanError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // D3：每张 image 一个 present-wait 信号量，数量与 image 集合一一对应，
        // 不与帧槽数挂钩——两帧在飞对三张 image 的错峰复用由"同 image 重 acquire"
        // 背书，不靠帧 fence 越权担保 present engine 的消费进度。
        let render_finished = images
            .iter()
            .map(|_| {
                unsafe { ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                    .map_err(VulkanError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;

        info!(
            "swapchain 就绪: {}x{}，{} images，底板 {:?} / 渲染 view {:?}，present={:?}，render_finished 按 image 配 {} 个",
            caps.current_extent.width,
            caps.current_extent.height,
            images.len(),
            surface_format.format,
            view_format,
            present_mode,
            render_finished.len(),
        );

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

    /// resize 后重建。两个幂等出口：
    /// - 最小化时 current_extent=0，建 0 尺寸 swapchain 非法——保留旧的，恢复后靠下一次 resize 消息重建；
    /// - 尺寸没变就不动。
    ///
    /// 单次真重建 ≈ 60ms（wait_idle+destroy+create，2026-09-22 实测）——调用方必须
    /// 去抖（host::draw_frame 的 `ResizeGate`），不要在拖拽的每个尺寸步进上调用。
    pub fn rebuild(&mut self, ctx: &Context) -> Result<()> {
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
        // 在飞帧还在引用旧 image，必须先等全部完成再拆。WSI 边界（缺陷文档 §4）：
        // wait_idle 只保证队列操作完成，不担保 presentation engine 消费完
        // render_finished；Win32 同一 hwnd 只容一个 swapchain，"先拆旧再建新"是
        // 强制时序，旧件延迟回收在本平台不可行——余量论证与严格方案
        //（present_wait / present_fence，待 V1 验证环境建立后评估）记录在缺陷文档。
        unsafe { ctx.device.device_wait_idle() }?;
        // Win32 的 WSI 限制：同一个 hwnd 同时只能挂一个 swapchain——
        // 必须先拆旧再建新（`*self = Self::new(ctx)` 的 RHS 先求值，是"先建后拆"，必炸）
        self.destroy();
        *self = Self::new(ctx)?; // 失败时 self 已是空壳（句柄为 null），下一帧 resize 消息进来重试
        Ok(())
    }

    /// 销毁 views + render_finished + swapchain（幂等：句柄清 null 后重复调用是空操作）。
    fn destroy(&mut self) {
        for view in self.views.drain(..) {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        // 信号量先于 swapchain（创建的逆序）：present 引用的是两者，销毁顺序本身
        // 不解除 present 在途的引用，安全边际由调用方的 wait_idle 时序承担（见 rebuild）
        for sem in self.render_finished.drain(..) {
            unsafe { self.device.destroy_semaphore(sem, None) };
        }
        if self.swapchain != vk::SwapchainKHR::null() {
            unsafe { self.fns.destroy_swapchain(self.swapchain, None) };
            self.swapchain = vk::SwapchainKHR::null();
        }
    }

    /// 取下一张可绘 image（信号量在 present engine 拿到图时置位）。
    /// `ERROR_OUT_OF_DATE_KHR` 折叠成 [`VulkanError::SwapchainOutOfDate`]。
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

    /// 呈现：等渲染提交的 render_finished 信号量。
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
        // view 先于 swapchain。Device/Instance 的存活由拆除顺序保证：
        // 正常退出走 driver::host 的 teardown_vulkan（FramePool → 本结构 → Context）；
        // panic 时 Resource drop 顺序不定，进程本就注定终止。
        self.destroy();
    }
}
