//! 图像资源层（施工 3.3.1）：bevy `Image` → VkImage/view/sampler，资产身份驻留缓存。
//!
//! 三条机制约束来自显存机制篇判定线（.agent/docs/3-静态取数链路/3.3-贴图与bindless描述符/
//! 显存机制：Buffer与Image之别、swizzle不透明与访问路径特化.md §0）：
//! 1. OPTIMAL tiling image 的内容不能按字节读写——上传走 `vkCmdCopyBufferToImage`
//!    （uploader.rs 的图像拷贝段），读回走 `vkCmdCopyImageToBuffer`；swizzle 重排由
//!    copy 引擎完成，应用永不参与。
//! 2. image 分配独立于 MeshPool：每张图一块 dedicated `VkDeviceMemory`（M2 张数少，
//!    `maxMemoryAllocationCount`（本机 4096）余量充足；memory 层子分配等显存压力出现再立）。
//! 3. layout 转换与 buffer 读写次序是两类问题——图像屏障携带 old/new layout 与子资源
//!    范围，uploader 里图像段独立录制，不套 buffer barrier 模板。
//!
//! 格式与角色：glTF loader 已按 texture 语义决定 sRGB/线性
//!（bevy_gltf/src/loader/mod.rs:1210 `is_srgb = !linear_textures.contains(..)`），
//! 落在 `Image.texture_descriptor.format` 的 Srgb 后缀上——本层只做格式核对与映射，
//! 不再二次推断角色。`ImageSampler::Default` 的解析：官方由 ImagePlugin 全局默认承接
//!（bevy_image/src/image.rs:181 `ImagePlugin::default()` = `ImageSamplerDescriptor::linear()`），
//! 禁渲染宿主里照该常量解析，不发明新默认。
//!
//! mip 策略：静态 mip0——只收单级贴图、只传 level 0，采样端以 `max_lod = 0` 钉死
//! 可采样 LOD（3.3.1 面板口径）；mip 链生成归步骤 5 之前。

use std::collections::HashMap;

use ash::{vk, Device, Instance};
use bevy::asset::AssetId;
use bevy::image::{
    Image as BevyImage, ImageAddressMode, ImageCompareFunction, ImageFilterMode, ImageSampler,
    ImageSamplerBorderColor, ImageSamplerDescriptor,
};
use bevy::log::info;
use thiserror::Error;
use wgpu_types::{TextureDimension, TextureFormat};

use super::resources::MemoryContract;
use super::uploader::Ticket;
use crate::error::VulkanError;

/// bevy `Image` → 渲染器规格的映射拒绝。Tier①：不影响帧循环，warn 一次跳过该资产
///（与 [`super::mesh_convert::MeshConvertError`] 同责）。
#[derive(Debug, Error)]
pub enum ImageConvertError {
    /// 只收 RGBA8（线性/sRGB）——FlightHelmet 全部 PNG 落在这两档；其余格式出现时
    /// 按 3.3.1"核对格式与使用角色"的口径显式拒绝，不静默猜。
    #[error("不支持的贴图格式 {0:?}（本步只收 Rgba8Unorm[Srgb]）")]
    UnsupportedFormat(TextureFormat),
    /// 没有 2D 网格就无法进采样路径。
    #[error("不支持的维度 {0:?}（本步只收 2D）")]
    UnsupportedDimension(TextureDimension),
    /// 静态 mip0 形状：mip 链生成归后续步骤，带链数据进来先拒绝（避免只取 mip0
    /// 还要解析 TextureDataOrder 排布的复杂度）。
    #[error("mip 链 {0} 级（本步静态 mip0，只收单级）")]
    UnsupportedMipLevels(u32),
    /// 未初始化的存储贴图没有可上传内容。
    #[error("贴图无像素数据（data 为 None）")]
    MissingData,
}

/// 一张贴图的渲染器规格：纯映射产物（可失败 = Tier①）；Vulkan 对象由
/// [`GpuImage::create`] 按此建，像素字节由调用方从 `Image.data` 直取进 staging。
#[derive(Debug)]
pub struct ImageSpec {
    pub width: u32,
    pub height: u32,
    /// 已核对的格式（创建时再查 optimal tiling feature 支持——支持与使用分开核）。
    pub format: vk::Format,
    /// 已解析的采样设置（`Default` → 官方 ImagePlugin 全局默认 linear()，见模块注释）。
    pub sampler: ImageSamplerDescriptor,
}

/// bevy `Image` → [`ImageSpec`]：核对维度/mip 形状/格式角色，解析采样设置。
///
/// # Errors
/// 维度非 2D、mip 非单级、格式不在 RGBA8 两档、data 缺失时返回 [`ImageConvertError`]。
pub fn image_spec(image: &BevyImage) -> Result<ImageSpec, ImageConvertError> {
    let desc = &image.texture_descriptor;
    if desc.dimension != TextureDimension::D2 {
        return Err(ImageConvertError::UnsupportedDimension(desc.dimension));
    }
    if desc.mip_level_count != 1 {
        return Err(ImageConvertError::UnsupportedMipLevels(desc.mip_level_count));
    }
    let format = match desc.format {
        TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
        TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        other => return Err(ImageConvertError::UnsupportedFormat(other)),
    };
    if image.data.is_none() {
        return Err(ImageConvertError::MissingData);
    }
    let sampler = match &image.sampler {
        // 出处与理由见模块注释:官方默认 = ImagePlugin::default() 的 linear()
        ImageSampler::Default => ImageSamplerDescriptor::linear(),
        ImageSampler::Descriptor(d) => d.clone(),
    };
    Ok(ImageSpec {
        width: image.width(),
        height: image.height(),
        format,
        sampler,
    })
}

impl ImageSpec {
    /// 本规格是否承担 sRGB 解码角色(base color/emissive 类;数据贴图为线性)。
    /// 角色判据 = 映射后的 SRGB 后缀格式,证据查询口。
    #[must_use]
    pub fn srgb_role(&self) -> bool {
        self.format == vk::Format::R8G8B8A8_SRGB
    }
}

/// 一张按契约创建并绑定的 2D 贴图：VkImage（OPTIMAL，UNDEFINED 起步）+ dedicated
/// memory + 全量 view + 采样器。layout 迁移与上传在 [`super::Uploader`] 的图像段，
/// 本类型只管资源本体——与 [`super::GpuBuffer`] 只管 buffer 本体同责。
pub struct GpuImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    format: vk::Format,
    width: u32,
    height: u32,
}

impl GpuImage {
    /// 按契约创建:image(usage 按读回条款三用途)→ requirements → 选型(必需
    /// DEVICE_LOCAL)→ 恰量分配 → offset 0 绑定 → view → 采样器。
    /// 任何失败回收已建对象(全有或全无)。
    ///
    /// # Errors
    /// 格式的 optimal tiling feature 不足,或 Vulkan 创建/分配/绑定失败。
    pub fn create(
        device: &Device,
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        contract: &MemoryContract,
        spec: &ImageSpec,
    ) -> Result<Self, VulkanError> {
        // 使用角色三用途:上传拷贝写(TRANSFER_DST)、读回校验拷贝读(TRANSFER_SRC,
        // 显存机制篇读回条款)、着色器采样(SAMPLED)。RGBA8 两档对这三用途都是
        // mandatory format support,但"必真"也要查过才用——证据落日志。
        let usage = vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;
        // usage 位 ↔ format feature 位的对应关系(三用途逐一换算后核对)
        let required_features = vk::FormatFeatureFlags::TRANSFER_DST
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        let features = unsafe {
            instance.get_physical_device_format_properties(physical_device, spec.format)
        }
        .optimal_tiling_features;
        if !features.contains(required_features) {
            return Err(VulkanError::Init(format!(
                "格式 {0:?} 的 optimal tiling features {features:?} 不含所需 {required_features:?},贴图无法按本步角色使用",
                spec.format
            )));
        }
        // # Safety:create_image 参数全为合法常量 + 核对过的格式/usage,无裸指针
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(spec.format)
                    .extent(vk::Extent3D {
                        width: spec.width,
                        height: spec.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    // EXCLUSIVE + 首用屏障落 transfer 族(Uploader 图像段);跨族让渡
                    // 走 release/acquire,与 3.2 buffer 同一机制,不用 CONCURRENT
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result =
            unsafe { Self::create_inner(device, contract, spec, usage, features, image) };
        // image 的回收统一收口在这里(内层只管 memory/view/sampler 的回收)
        if result.is_err() {
            unsafe { device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链的后半段(image 已建):失败路径回收已分配的 memory / 已建的 view、sampler。
    ///
    /// # Safety
    /// `image` 须是刚创建成功的合法句柄;错误返回后调用方不得再使用它。
    unsafe fn create_inner(
        device: &Device,
        contract: &MemoryContract,
        spec: &ImageSpec,
        usage: vk::ImageUsageFlags,
        features: vk::FormatFeatureFlags,
        image: vk::Image,
    ) -> Result<Self, VulkanError> {
        unsafe {
            // 绑定三条款的证据来源:requirements 三元组,一次查询全用上。
            // dedicated 分配(显存机制篇 §6:M2 形态;子分配等显存压力出现再立)。
            let reqs = device.get_image_memory_requirements(image);
            let memory_type = contract.find_type(
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::empty(),
            )?;
            let memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            if let Err(e) = device.bind_image_memory(image, memory, 0) {
                device.free_memory(memory, None);
                return Err(VulkanError::Vk(e));
            }
            // view:全量 COLOR 子资源、mip0 单层——与上传/采样的子资源范围一致
            let view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(spec.format)
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
                    device.free_memory(memory, None);
                    return Err(VulkanError::Vk(e));
                }
            };
            let sampler = match Self::create_sampler(device, &spec.sampler) {
                Ok(s) => s,
                Err(e) => {
                    device.destroy_image_view(view, None);
                    device.free_memory(memory, None);
                    return Err(e);
                }
            };
            let heap = contract.heap_of_type(memory_type);
            info!(
                "贴图按契约就绪: {0:?} {1}×{2} → requirements 分配 {reqs_size}B(对齐 {reqs_alignment}B), \
                 内存类型 {memory_type} 堆 {heap_index}({heap_size}B, {heap_flags:?}), \
                 usage {usage:?} 全含于 optimal features {features:?}",
                spec.format,
                spec.width,
                spec.height,
                reqs_size = reqs.size,
                reqs_alignment = reqs.alignment,
                heap_index = contract.heap_index_of_type(memory_type),
                heap_size = heap.size,
                heap_flags = heap.flags,
            );
            Ok(Self {
                device: device.clone(),
                image,
                memory,
                view,
                sampler,
                format: spec.format,
                width: spec.width,
                height: spec.height,
            })
        }
    }

    /// bevy 采样描述符 → VkSampler。lod 上限钉 0:静态 mip0 模式的可采样 LOD 限制
    ///（3.3.1 口径)——描述符里的 lod_max_clamp(默认 32)按本步策略覆盖为 0;
    /// anisotropy 需要独立 feature,本步不开,描述符里的 clamp 一律不启用。
    ///
    /// # Errors
    /// `vkCreateSampler` 失败。
    fn create_sampler(
        device: &Device,
        d: &ImageSamplerDescriptor,
    ) -> Result<vk::Sampler, VulkanError> {
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(filter(d.mag_filter))
            .min_filter(filter(d.min_filter))
            .mipmap_mode(mipmap_mode(d.mipmap_filter))
            .address_mode_u(address_mode(d.address_mode_u))
            .address_mode_v(address_mode(d.address_mode_v))
            .address_mode_w(address_mode(d.address_mode_w))
            .mip_lod_bias(0.0)
            .min_lod(0.0)
            .max_lod(0.0)
            .compare_enable(d.compare.is_some())
            .compare_op(d.compare.map_or(vk::CompareOp::NEVER, compare_op))
            .border_color(d.border_color.map_or(vk::BorderColor::FLOAT_TRANSPARENT_BLACK, border_color))
            // anisotropy_enable false 时 clamp 值不参与采样,无需特性
            .anisotropy_enable(false);
        unsafe { device.create_sampler(&info, None) }.map_err(VulkanError::from)
    }

    /// 绑定的 Vulkan image 句柄(上传拷贝的目标、读回拷贝的源)。
    #[must_use]
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// 全量 view 句柄(3.3.3 描述符表从这里取)。
    #[must_use]
    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    /// 采样器句柄(3.3.3 描述符表从这里取;M2 每图一个,去重策略归 3.3.4 待决)。
    #[must_use]
    pub fn sampler(&self) -> vk::Sampler {
        self.sampler
    }

    /// 绑定的格式(证据查询口;3.3.3 描述符布局与此一致)。
    #[must_use]
    pub fn format(&self) -> vk::Format {
        self.format
    }

    /// 尺寸(证据查询口)。
    #[must_use]
    pub fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

// # Safety:GpuImage 持有的 ash 句柄本质是整数,不指向 Rust 内存;多线程共享由
// 外部纪律保证(与 GpuBuffer 同款约定:资源创建/销毁只在持有方,方法只读句柄)
unsafe impl Send for GpuImage {}
unsafe impl Sync for GpuImage {}

impl Drop for GpuImage {
    fn drop(&mut self) {
        // 契约边界(与 GpuBuffer/Uploader 同款):不等 GPU;退出排空由 teardown_vulkan
        // 的 device_wait_idle 先于一切资源 Drop(D4 定案)。销毁序:采样器/view 先于
        // image,memory 最后(image 销毁后再释放其绑定)。
        unsafe {
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// `bevy::image::Image` 资产身份 → 驻留票据。与 [`super::MeshPool`] 的驻留缓存同责:
/// 每帧快照带着同一批柄来,只有"查无此身份"的才创建上传(重复快照零重复上传);
/// 票据记录"这份数据何时可用"。槽位分配/发布是 3.3.4 的事,本类型不排槽。
#[derive(bevy::prelude::Resource, Default)]
pub struct ImageCache {
    resident: HashMap<AssetId<BevyImage>, (GpuImage, Ticket)>,
}

impl ImageCache {
    /// 身份是否已驻留;在则返回其上传完成票据(消费方等待的凭据)。
    #[must_use]
    pub fn resident(&self, id: AssetId<BevyImage>) -> Option<Ticket> {
        self.resident.get(&id).map(|(_, ticket)| *ticket)
    }

    /// 已驻留贴图数(日志与"全部驻留"收账用)。
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    /// 驻留登记:仅在对应批次提交成功后调用(失败路径不留账本行,与 pool.commit 同责)。
    pub fn commit(&mut self, id: AssetId<BevyImage>, image: GpuImage, ticket: Ticket) {
        self.resident.insert(id, (image, ticket));
    }

    /// 按身份取驻留贴图(3.3.3 描述符表、3.4 DrawList 的取数口)。
    #[must_use]
    pub fn image(&self, id: AssetId<BevyImage>) -> Option<&GpuImage> {
        self.resident.get(&id).map(|(image, _)| image)
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
