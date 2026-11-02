//! 颜色渲染目标：image + 独占内存 + view。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::context::Context;

use super::attachment;

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

    /// 建颜色目标：格式须支持颜色附件 + 采样，`need_blend` 时还须支持混合；任一步失败回收已建对象。
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
        // 要接透明层混合的目标须支持混合（HDR_FORMAT 规范必支持，仍照查）
        if need_blend && !features.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND) {
            return Err(VulkanError::Init(format!(
                "{role} 格式 {format:?} 不支持 COLOR_ATTACHMENT_BLEND，透明层 blending 前置检查不通过"
            )));
        }
        let a = attachment::create(
            ctx,
            extent,
            format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        Ok(Self {
            device: ctx.device.clone(),
            image: a.image,
            memory: a.memory,
            view: a.view,
        })
    }
}

impl Drop for ColorTarget {
    fn drop(&mut self) {
        // 不等 GPU：重建路径的等待在 rebuild，退出排空在 teardown
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
