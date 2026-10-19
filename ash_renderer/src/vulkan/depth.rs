//! 深度附件：D32_SFLOAT reverse-Z 深度链的载体（3.6 P1 摊薄兑现：从 frames.rs
//! 独立成侧文件——生命周期归帧槽，职责单一）。
//!
//! 支持查询先行（支持与启用分开，证据落日志）；每帧槽一张，尺寸随 swapchain
//! 重建（[`FramePool::rebuild_depth`]，在 frames.rs）。销毁纪律：Drop 不等 GPU
//! ——重建路径的等待在 rebuild_depth，退出排空在 teardown。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};

use super::Context;

/// 深度附件格式（D32_SFLOAT，reverse-Z 深度链的载体）。支持查询在
/// [`DepthTarget::new`]；管线的深度附件格式声明与这里同一常量
///（pipeline.rs 只消费不另立）。
pub const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

/// 一张深度附件：VkImage（D32_SFLOAT，DEPTH_STENCIL_ATTACHMENT + SAMPLED
/// 用途——6.1.2 起金字塔归约采样）+ dedicated memory + view（DEPTH aspect）。
/// 生命周期 = 帧槽，重建见 `FramePool::rebuild_depth`。
pub struct DepthTarget {
    device: Device,
    /// 本体 image（进场屏障的目标；FramePool 经 `raw_image` 访问）。
    pub(crate) image: vk::Image,
    memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl DepthTarget {
    /// 按契约创建深度附件。格式支持先查后用（支持与启用分开，1.3 实现对
    /// D32_SFLOAT 的 depth-stencil attachment 支持是 mandatory，仍不省查询——
    /// 证据落日志）。
    ///
    /// 6.1.2 起 usage 扩 SAMPLED：Hi-Z 金字塔归约在本帧渲染后采样本帧槽深度
    ///（hiz 模块）。D32_SFLOAT 的 SAMPLED_IMAGE 位同样是 mandatory 格式支持，
    /// 仍查过才用（"支持≠启用"纪律，立案期核实的 usage 缺口在此收口）。
    ///
    /// # Errors
    /// D32_SFORMAT 的 optimal tiling feature 不足，或 Vulkan 创建/分配/绑定失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D) -> Result<Self> {
        let format = DEPTH_FORMAT;
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, format)
        }
        .optimal_tiling_features;
        if !features.contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT) {
            return Err(VulkanError::Init(format!(
                "深度格式 {format:?} 的 optimal tiling features {features:?} 不含 DEPTH_STENCIL_ATTACHMENT，reverse-Z 深度链无法建立"
            )));
        }
        if !features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE) {
            return Err(VulkanError::Init(format!(
                "深度格式 {format:?} 的 optimal tiling features {features:?} 不含 SAMPLED_IMAGE，Hi-Z 归约无法采样深度（§2 核实项 1 出局）"
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
                    .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(ctx, image) };
        if result.is_err() {
            unsafe { ctx.device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链后半段(image 已建):失败路径回收 memory。
    ///
    /// # Safety
    /// `image` 须刚创建成功且未销毁;错误返回后调用方不得再使用它。
    unsafe fn create_inner(ctx: &Context, image: vk::Image) -> Result<Self> {
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
            // view:DEPTH aspect（D32_SFLOAT 无 stencil 面）、mip0 单层
            let view = ctx.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(DEPTH_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
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
            info!(
                "深度附件就绪: D32_SFLOAT，内存类型 {memory_type}（DEVICE_LOCAL），requirements 分配 {size}B",
                size = reqs.size,
            );
            Ok(Self {
                device: ctx.device.clone(),
                image,
                memory,
                view,
            })
        }
    }
}

impl Drop for DepthTarget {
    fn drop(&mut self) {
        // 契约边界:不等 GPU(重建路径的等待在 rebuild_depth,退出排空在 teardown)。
        // view 先于 image,memory 最后。
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
