//! 采样器参数键：bevy `ImageSamplerDescriptor` → 可哈希的 [`SamplerKey`]，再按键建 VkSampler。
//! bindless 表按键去重共享采样器槽，后处理 pass 也用它建自己的采样器。
//!
//! 所有采样器 `max_lod` 放开到 `VK_LOD_CLAMP_NONE`，可用 mip 级数由 view 决定；
//! min/mag 都双线性的采样器统一开各向异性（目标 8×，受设备上限约束），
//! 地面/铺装这类高频贴图在掠射角不会退化成摩尔纹。

use ash::{vk, Device};
use bevy::image::{
    ImageAddressMode, ImageCompareFunction, ImageFilterMode, ImageSamplerBorderColor,
    ImageSamplerDescriptor,
};

use crate::common::error::{Result, VulkanError};

/// 各向异性目标倍数：8× 是锐度和带宽的常用折中，实际取值再按设备上限收缩。
const TARGET_ANISOTROPY: f32 = 8.0;

/// 采样器去重键：VkSampler 的全部可变参数（滤波两轴 + mip 模式、寻址三轴、比较、边框色）。
/// 同键即同一配方，可共享一个描述符槽。不进键的：
/// - 格式：VkSampler 没有格式参数（sRGB/线性由 image view 决定）；
/// - lod/anisotropy/bias：[`SamplerKey::create_sampler`] 对所有采样器取同一策略
///   （各向异性由设备上限和滤波共同决定，见 [`anisotropy_settings`]）。
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
    /// （可用级数由 view 决定，单级贴图同样适用）、bias 0、各向异性按 [`anisotropy_settings`]
    /// 取目标 8× 与设备上限的较小值。
    ///
    /// `device_max_anisotropy` 取设备的 `maxSamplerAnisotropy`（`samplerAnisotropy` feature
    /// 不支持时传 1.0，此时各向异性保持关闭）。
    ///
    /// # Errors
    /// `vkCreateSampler` 失败。
    pub(crate) fn create_sampler(
        &self,
        device: &Device,
        device_max_anisotropy: f32,
    ) -> Result<vk::Sampler> {
        let (anisotropy_enable, max_anisotropy) = anisotropy_settings(
            device_max_anisotropy,
            vk::Filter::from_raw(self.mag),
            vk::Filter::from_raw(self.min),
        );
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
            .anisotropy_enable(anisotropy_enable)
            .max_anisotropy(max_anisotropy);
        unsafe { device.create_sampler(&info, None) }.map_err(VulkanError::from)
    }
}

/// 各向异性过滤统一策略：目标 [`TARGET_ANISOTROPY`]（8×），按设备 `maxSamplerAnisotropy`
/// 上限收缩；设备上限 ≤ 1（不支持各向异性）或 min/mag 任一不是双线性时关闭。
///
/// 为什么开：地面、铺装、格栅这类高频贴图在掠射角被三线性压到低 mip，各向同性滤波会出
/// 摩尔纹/糊成一片；各向异性沿视线方向多采样几个 tap 才能保住细节。
///
/// 坑：开各向异性要求 `samplerAnisotropy` feature 已启用
/// （VUID-VkSamplerCreateInfo-anisotropyEnable-01070），所以设备上限为 1 时必须关闭。
/// 最近邻采样器也保持关闭：点采样没有沿视线的过滤收益，只会多付采样成本。
#[must_use]
pub(crate) fn anisotropy_settings(
    device_max: f32,
    mag: vk::Filter,
    min: vk::Filter,
) -> (bool, f32) {
    if device_max <= 1.0 || mag != vk::Filter::LINEAR || min != vk::Filter::LINEAR {
        return (false, 1.0);
    }
    (true, TARGET_ANISOTROPY.clamp(1.0, device_max))
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
