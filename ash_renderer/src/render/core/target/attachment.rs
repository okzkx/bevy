//! 附件图像的公共创建链：2D 单 mip 单层 image → 独占 DEVICE_LOCAL 内存 → 全图 view。
//! 颜色目标与深度目标共用；格式支持由调用方先查，任一步失败回收已建对象。

use ash::vk;

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::context::Context;

/// 建成的附件句柄，归调用方持有，按 view → image → memory 顺序销毁。
pub(super) struct Attachment {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    /// 选中的内存类型（日志用）。
    pub memory_type: u32,
    /// requirements 分配字节数（日志用）。
    pub size: u64,
}

/// # Errors
/// 没有 DEVICE_LOCAL 内存类型，或 Vulkan 创建/分配/绑定失败。
pub(super) fn create(
    ctx: &Context,
    extent: vk::Extent2D,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    aspect: vk::ImageAspectFlags,
) -> Result<Attachment> {
    // # Safety:create_image 参数全为合法常量 + 调用方查过的格式/usage,无裸指针
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
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }?;
    let result = unsafe { bind_and_view(ctx, image, format, aspect) };
    if result.is_err() {
        unsafe { ctx.device.destroy_image(image, None) };
    }
    result
}

/// 创建链后半段（image 已建）：失败路径回收 memory。
///
/// # Safety
/// `image` 须刚创建成功且未销毁；错误返回后调用方不得再使用它。
unsafe fn bind_and_view(
    ctx: &Context,
    image: vk::Image,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
) -> Result<Attachment> {
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
        let view = ctx.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(aspect)
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
        Ok(Attachment {
            image,
            memory,
            view,
            memory_type,
            size: reqs.size,
        })
    }
}
