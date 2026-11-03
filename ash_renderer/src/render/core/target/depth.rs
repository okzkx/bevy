//! 深度目标：D32_SFLOAT、reverse-Z。主深度每帧槽一张，随 swapchain 重建
//!（[`crate::render::FramePool::rebuild_depth`]）；阴影的四张级联深度图也用它。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::context::Context;

use super::attachment;

/// 所有管线声明深度附件格式都用这个常量，不另写。
pub const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

/// 一张可当深度附件、也可被采样（合成、Hi-Z、阴影查询）的深度图，独占一块内存。
pub struct DepthTarget {
    device: Device,
    /// 本体 image（布局屏障用）。
    pub(crate) image: vk::Image,
    memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl DepthTarget {
    /// 建深度目标：格式须支持深度附件与采样（Hi-Z 归约、合成要采样深度）；D32_SFLOAT
    /// 这两项都是规范必支持，仍照查。
    ///
    /// # Errors
    /// D32_SFLOAT 的 optimal tiling feature 不足，或 Vulkan 创建/分配/绑定失败。
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
                "深度格式 {format:?} 的 optimal tiling features {features:?} 不含 SAMPLED_IMAGE，Hi-Z 归约无法采样深度"
            )));
        }
        // DEPTH aspect：D32_SFLOAT 没有 stencil 面
        let a = attachment::create(
            ctx,
            extent,
            format,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::DEPTH,
        )?;
        info!(
            "深度附件就绪: D32_SFLOAT，内存类型 {memory_type}（DEVICE_LOCAL），requirements 分配 {size}B",
            memory_type = a.memory_type,
            size = a.size,
        );
        Ok(Self {
            device: ctx.device.clone(),
            image: a.image,
            memory: a.memory,
            view: a.view,
        })
    }
}

impl Drop for DepthTarget {
    fn drop(&mut self) {
        // 不等 GPU：重建前由 rebuild_depth 等，退出前由 teardown 等。
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
