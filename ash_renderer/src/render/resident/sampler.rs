//! 采样器参数键：bevy `ImageSamplerDescriptor` → 可哈希的 [`SamplerKey`]，再按键建 VkSampler。
//! bindless 表按键去重共享采样器槽，后处理 pass 也用它建自己的采样器。
//!
//! 所有采样器 `max_lod` 放开到 `VK_LOD_CLAMP_NONE`，可用 mip 级数由 view 决定。

use ash::{vk, Device};
use bevy::image::{
    ImageAddressMode, ImageCompareFunction, ImageFilterMode, ImageSamplerBorderColor,
    ImageSamplerDescriptor,
};

use crate::common::error::{Result, VulkanError};

/// 采样器去重键：VkSampler 的全部可变参数（滤波两轴 + mip 模式、寻址三轴、比较、边框色）。
/// 同键即同一配方，可共享一个描述符槽。不进键的：
/// - 格式：VkSampler 没有格式参数（sRGB/线性由 image view 决定）；
/// - lod/anisotropy/bias：[`SamplerKey::create_sampler`] 对所有采样器取同值。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SamplerKey {
    mag: i32,
    min: i32,
    mip: i32,
    u: i32,
    v: i32,
    w: i32,
    /// None = 比较关闭（与 Some(NEVER) 的 compare_enable=true 不同）
    compare: Option<i32>,
    border: i32,
}

/// bevy 采样描述符 → [`SamplerKey`]（与 [`SamplerKey::create_sampler`] 的参数一一对应，改一处
/// 必须改另一处）。
#[must_use]
pub fn sampler_key(d: &ImageSamplerDescriptor) -> SamplerKey {
    SamplerKey {
        mag: filter(d.mag_filter).as_raw(),
        min: filter(d.min_filter).as_raw(),
        mip: mipmap_mode(d.mipmap_filter).as_raw(),
        u: address_mode(d.address_mode_u).as_raw(),
        v: address_mode(d.address_mode_v).as_raw(),
        w: address_mode(d.address_mode_w).as_raw(),
        compare: d.compare.map(|c| compare_op(c).as_raw()),
        border: d
            .border_color
            .map_or(vk::BorderColor::FLOAT_TRANSPARENT_BLACK, border_color)
            .as_raw(),
    }
}

impl SamplerKey {
    /// 按键建 VkSampler（句柄归调用方所有）。所有采样器统一：`max_lod = VK_LOD_CLAMP_NONE`
    /// （可用级数由 view 决定，单级贴图同样适用）、不开 anisotropy、bias 0。
    ///
    /// # Errors
    /// `vkCreateSampler` 失败。
    pub(crate) fn create_sampler(&self, device: &Device) -> Result<vk::Sampler> {
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::from_raw(self.mag))
            .min_filter(vk::Filter::from_raw(self.min))
            .mipmap_mode(vk::SamplerMipmapMode::from_raw(self.mip))
            .address_mode_u(vk::SamplerAddressMode::from_raw(self.u))
            .address_mode_v(vk::SamplerAddressMode::from_raw(self.v))
            .address_mode_w(vk::SamplerAddressMode::from_raw(self.w))
            .mip_lod_bias(0.0)
            .min_lod(0.0)
            .max_lod(vk::LOD_CLAMP_NONE)
            .compare_enable(self.compare.is_some())
            .compare_op(
                self.compare
                    .map_or(vk::CompareOp::NEVER, vk::CompareOp::from_raw),
            )
            .border_color(vk::BorderColor::from_raw(self.border))
            .anisotropy_enable(false);
        unsafe { device.create_sampler(&info, None) }.map_err(VulkanError::from)
    }
}

fn filter(f: ImageFilterMode) -> vk::Filter {
    match f {
        ImageFilterMode::Nearest => vk::Filter::NEAREST,
        ImageFilterMode::Linear => vk::Filter::LINEAR,
    }
}

fn mipmap_mode(f: ImageFilterMode) -> vk::SamplerMipmapMode {
    match f {
        ImageFilterMode::Nearest => vk::SamplerMipmapMode::NEAREST,
        ImageFilterMode::Linear => vk::SamplerMipmapMode::LINEAR,
    }
}

fn address_mode(m: ImageAddressMode) -> vk::SamplerAddressMode {
    match m {
        ImageAddressMode::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
        ImageAddressMode::Repeat => vk::SamplerAddressMode::REPEAT,
        ImageAddressMode::MirrorRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
        ImageAddressMode::ClampToBorder => vk::SamplerAddressMode::CLAMP_TO_BORDER,
    }
}

fn compare_op(op: ImageCompareFunction) -> vk::CompareOp {
    match op {
        ImageCompareFunction::Never => vk::CompareOp::NEVER,
        ImageCompareFunction::Less => vk::CompareOp::LESS,
        ImageCompareFunction::Equal => vk::CompareOp::EQUAL,
        ImageCompareFunction::LessEqual => vk::CompareOp::LESS_OR_EQUAL,
        ImageCompareFunction::Greater => vk::CompareOp::GREATER,
        ImageCompareFunction::NotEqual => vk::CompareOp::NOT_EQUAL,
        ImageCompareFunction::GreaterEqual => vk::CompareOp::GREATER_OR_EQUAL,
        ImageCompareFunction::Always => vk::CompareOp::ALWAYS,
    }
}

fn border_color(c: ImageSamplerBorderColor) -> vk::BorderColor {
    match c {
        ImageSamplerBorderColor::TransparentBlack => vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
        ImageSamplerBorderColor::OpaqueBlack => vk::BorderColor::FLOAT_OPAQUE_BLACK,
        ImageSamplerBorderColor::OpaqueWhite => vk::BorderColor::FLOAT_OPAQUE_WHITE,
        ImageSamplerBorderColor::Zero => vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
    }
}
