//! 贴图规格：[`ImageSpec`] 是 [`super::images::GpuImage::create`] 的输入（宽高、格式、mip、采样）。
//! [`image_spec`] 从 bevy `Image` 核对并换算出规格，纯 CPU，不碰像素字节——上传时调用方
//! 直接从 `Image.data` 取 mip0。非资产贴图（egui 图集、SMAA 查找表、fallback 白图）直接手写规格。
//!
//! sRGB/线性由 glTF loader 按贴图用途决定，体现在 `Image.texture_descriptor.format` 的 Srgb
//! 后缀上；本层只核对格式并映射，不二次推断。`ImageSampler::Default` 按 bevy `ImagePlugin`
//! 的默认值解析为 `ImageSamplerDescriptor::linear()`。

use ash::vk;
use bevy::image::{Image as BevyImage, ImageSampler, ImageSamplerDescriptor};
use thiserror::Error;
use wgpu_types::{TextureDimension, TextureFormat};

/// bevy `Image` → 渲染器规格的映射拒绝：不影响帧循环，调用方 warn 后跳过该资产
/// （同 [`super::mesh_convert::MeshConvertError`]）。
#[derive(Debug, Error)]
pub enum ImageSpecError {
    /// 只收 RGBA8（线性/sRGB），其余格式显式拒绝，不猜。
    #[error("不支持的贴图格式 {0:?}（只收 Rgba8Unorm[Srgb]）")]
    UnsupportedFormat(TextureFormat),
    #[error("不支持的维度 {0:?}（只收 2D）")]
    UnsupportedDimension(TextureDimension),
    /// 未初始化的存储贴图没有可上传内容。
    #[error("贴图无像素数据（data 为 None）")]
    MissingData,
}

/// 一张贴图的渲染器规格。Vulkan 对象由 [`GpuImage::create`](super::images::GpuImage::create)
/// 按它建，像素字节由调用方从 `Image.data` 直接取进 staging（只取 mip0）。
#[derive(Debug)]
pub struct ImageSpec {
    pub width: u32,
    pub height: u32,
    /// 已核对的格式（创建时再查 optimal tiling feature）。
    pub format: vk::Format,
    /// 全链级数；实际建几级由 [`GpuImage::create`](super::images::GpuImage::create) 按格式
    /// blit 能力决定，不支持则降为 1。
    pub mip_levels: u32,
    /// 已解析的采样设置（`Default` → `linear()`）。
    pub sampler: ImageSamplerDescriptor,
}

/// bevy `Image` → [`ImageSpec`]：核对维度/格式角色，解析采样设置，算全链级数。
///
/// # Errors
/// 维度非 2D、格式不在 RGBA8 两档、data 缺失时返回 [`ImageSpecError`]。
pub fn image_spec(image: &BevyImage) -> Result<ImageSpec, ImageSpecError> {
    let desc = &image.texture_descriptor;
    if desc.dimension != TextureDimension::D2 {
        return Err(ImageSpecError::UnsupportedDimension(desc.dimension));
    }
    let format = match desc.format {
        TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
        TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        other => return Err(ImageSpecError::UnsupportedFormat(other)),
    };
    if image.data.is_none() {
        return Err(ImageSpecError::MissingData);
    }
    let sampler = match &image.sampler {
        ImageSampler::Default => ImageSamplerDescriptor::linear(),
        ImageSampler::Descriptor(d) => d.clone(),
    };
    Ok(ImageSpec {
        width: image.width(),
        height: image.height(),
        format,
        mip_levels: full_mip_chain(image.width(), image.height()),
        sampler,
    })
}

/// 2D 贴图的全 mip 链级数：`floor(log2(max(w,h))) + 1`（1×1 → 1 级，1024×256 → 11 级）。
#[must_use]
pub fn full_mip_chain(width: u32, height: u32) -> u32 {
    let m = width.max(height).max(1);
    (u32::BITS - m.leading_zeros()).max(1)
}

impl ImageSpec {
    /// 是否按 sRGB 解码（base color/emissive 类；数据贴图为线性），按映射后的格式判断。
    #[must_use]
    pub fn srgb_role(&self) -> bool {
        self.format == vk::Format::R8G8B8A8_SRGB
    }
}

#[cfg(test)]
mod tests {
    use super::full_mip_chain;

    #[test]
    fn mip_chain_levels() {
        assert_eq!(full_mip_chain(1, 1), 1);
        assert_eq!(full_mip_chain(2, 2), 2);
        assert_eq!(full_mip_chain(4, 2), 3);
        assert_eq!(full_mip_chain(1024, 256), 11);
        assert_eq!(full_mip_chain(6, 6), 3);
    }
}
