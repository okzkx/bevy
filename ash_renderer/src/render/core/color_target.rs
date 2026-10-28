//! 颜色渲染目标：image + 独占内存 + view。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::Context;

/// HDR 中间目标格式：合成、透明层（alpha 混合）和 Bloom 链都写这个格式。
pub const HDR_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// 一张可当颜色附件、也可被采样的图像，独占一块内存。G-buffer、Bloom、SMAA
/// 的中间目标都用它。
pub(crate) struct ColorTarget {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl ColorTarget {
    /// image 句柄（布局屏障用）。
    #[must_use]
    pub(crate) fn image(&self) -> vk::Image {
        self.image
    }

    /// view 句柄（附件/采样描述符用）。
    #[must_use]
    pub(crate) fn view(&self) -> vk::ImageView {
        self.view
    }

}

impl ColorTarget {
    /// 按契约创建颜色附件：格式 feature 逐位查证（支持与启用分开），全有或全无。
    ///
    /// # Errors
    /// 格式 feature 不足或 Vulkan 创建/分配/绑定失败。
    pub(crate) fn new(
        ctx: &Context,
        extent: vk::Extent2D,
        format: vk::Format,
        need_blend: bool,
        role: &str,
    ) -> Result<Self> {
        let _ = extent;
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, format)
        }
        .optimal_tiling_features;
        let required =
            vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        if !features.contains(required) {
            return Err(VulkanError::Init(format!(
                "{role} 格式 {format:?} 的 optimal tiling features {features:?} 不含 {required:?}，G-buffer 无法按该使用角色建立"
            )));
        }
        // HDR 契约前置闸门（步骤级计划 §1 8.1）：中间目标要接透明层 blending
        //——支持位实测落日志（该格式 blending 核心规范 mandatory，仍不省查询）
        if need_blend && !features.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND) {
            return Err(VulkanError::Init(format!(
                "{role} 格式 {format:?} 不支持 COLOR_ATTACHMENT_BLEND，透明层 blending 前置闸门不通过"
            )));
        }
        // # Safety:create_image 参数全为合法常量 + 查过的格式/usage,无裸指针
        let image = unsafe {
            ctx.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: extent.width,
                        height: extent.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(ctx, image, format) };
        if result.is_err() {
            unsafe { ctx.device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链后半段（image 已建）：失败路径回收 memory/view。
    ///
    /// # Safety
    /// `image` 须刚创建成功且未销毁；错误返回后调用方不得再使用它。
    unsafe fn create_inner(ctx: &Context, image: vk::Image, format: vk::Format) -> Result<Self> {
        unsafe {
            let reqs = ctx.device.get_image_memory_requirements(image);
            let memory_type = ctx.memory_contract().find_type(
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::empty(),
            )?;
            let memory = ctx.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            if let Err(e) = ctx.device.bind_image_memory(image, memory, 0) {
                ctx.device.free_memory(memory, None);
                return Err(VulkanError::Vk(e));
            }
            // view:COLOR aspect、mip0 单层
            let view = ctx.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            );
            let view = match view {
                Ok(v) => v,
                Err(e) => {
                    ctx.device.free_memory(memory, None);
                    return Err(VulkanError::Vk(e));
                }
            };
            Ok(Self {
                device: ctx.device.clone(),
                image,
                memory,
                view,
            })
        }
    }
}

impl Drop for ColorTarget {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——重建路径的等待在 rebuild，退出排空在 teardown
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
