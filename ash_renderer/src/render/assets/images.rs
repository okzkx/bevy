//! 图像资源层：bevy `Image` → VkImage/view/sampler，资产身份驻留缓存。
//!
//! 三条机制约束来自显存机制篇判定线（.agents/docs/3-静态取数链路/3.3-贴图与bindless描述符/
//! 显存机制：Buffer与Image之别、swizzle不透明与访问路径特化.md §0）：
//! 1. OPTIMAL tiling image 的内容不能按字节读写——上传走 `vkCmdCopyBufferToImage`
//!    （uploader.rs 的图像拷贝段），读回走 `vkCmdCopyImageToBuffer`；swizzle 重排由
//!    copy 引擎完成，应用永不参与。
//! 2. image 分配独立于 MeshPool：每张图一块 dedicated `VkDeviceMemory`（贴图张数少，
//!    `maxMemoryAllocationCount`（本机 4096）余量充足；memory 层子分配等显存压力出现再立）。
//! 3. layout 转换与 buffer 读写次序是两类问题——图像屏障携带 old/new layout 与子资源
//!    范围，uploader 里图像段独立录制，不套 buffer barrier 模板。
//!
//! 格式与角色：glTF loader 已按 texture 语义决定 sRGB/线性
//!（bevy_gltf/src/loader/mod.rs:1210 `is_srgb = !linear_textures.contains(..)`），
//! 落在 `Image.texture_descriptor.format` 的 Srgb 后缀上——本层只做格式核对与映射，
//! 不二次推断角色。`ImageSampler::Default` 的解析：官方由 ImagePlugin 全局默认承接
//!（bevy_image/src/image.rs:181 `ImagePlugin::default()` = `ImageSamplerDescriptor::linear()`），
//! 禁渲染宿主里照该常量解析，不发明新默认。
//!
//! mip 策略（5.4.3 起修订，静态 mip0 退役）：资产侧数据按 mip0 收（图片
//! 加载器只给 level 0，带链数据解析不归本层——TextureDataOrder 排布复杂度
//! 不引入），mip 链由 GPU blit 逐级缩生成（传输批内完成，一次性非每帧）；
//! 采样端 `max_lod` 放开到 `VK_LOD_CLAMP_NONE`，可用 LOD 上界 = 实际链级数。
//! 格式缺 blit/线性过滤支持的贴图降级单级创建（能力判定在
//! [`GpuImage::create`]，降级有日志）。

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

use crate::render::resources::MemoryContract;
use super::uploader::{Ticket, Uploader};
use crate::common::error::{Result, VulkanError};

/// bevy `Image` → 渲染器规格的映射拒绝。Tier①：不影响帧循环，warn 一次跳过该资产
///（与 [`super::mesh_convert::MeshConvertError`] 同责）。
#[derive(Debug, Error)]
pub enum ImageConvertError {
    /// 只收 RGBA8（线性/sRGB）——FlightHelmet 全部 PNG 落在这两档；其余格式出现时
    /// 按"核对格式与使用角色"的口径显式拒绝，不静默猜。
    #[error("不支持的贴图格式 {0:?}（只收 Rgba8Unorm[Srgb]）")]
    UnsupportedFormat(TextureFormat),
    /// 没有 2D 网格就无法进采样路径。
    #[error("不支持的维度 {0:?}（只收 2D）")]
    UnsupportedDimension(TextureDimension),
    /// 未初始化的存储贴图没有可上传内容。
    #[error("贴图无像素数据（data 为 None）")]
    MissingData,
}

/// 一张贴图的渲染器规格：纯映射产物（可失败 = Tier①）；Vulkan 对象由
/// [`GpuImage::create`] 按此建，像素字节由调用方从 `Image.data` 直取进 staging
///（只取 mip0，见模块注释）。
#[derive(Debug)]
pub struct ImageSpec {
    pub width: u32,
    pub height: u32,
    /// 已核对的格式（创建时再查 optimal tiling feature 支持——支持与使用分开核）。
    pub format: vk::Format,
    /// 全链级数（`floor(log2(max(w,h))) + 1`；实际建几级由
    /// [`GpuImage::create`] 的格式 blit 能力定，缺支持降级 1）。
    pub mip_levels: u32,
    /// 已解析的采样设置（`Default` → 官方 ImagePlugin 全局默认 linear()，见模块注释）。
    pub sampler: ImageSamplerDescriptor,
}

/// bevy `Image` → [`ImageSpec`]：核对维度/格式角色，解析采样设置，算全链级数。
///
/// # Errors
/// 维度非 2D、格式不在 RGBA8 两档、data 缺失时返回 [`ImageConvertError`]。
pub fn image_spec(image: &BevyImage) -> Result<ImageSpec, ImageConvertError> {
    let desc = &image.texture_descriptor;
    if desc.dimension != TextureDimension::D2 {
        return Err(ImageConvertError::UnsupportedDimension(desc.dimension));
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
        mip_levels: full_mip_chain(image.width(), image.height()),
        sampler,
    })
}

/// 2D 贴图的全 mip 链级数：`floor(log2(max(w,h))) + 1`（1×1 → 1 级，
/// 1024×256 → 11 级）。VkImage 的 mip_levels 与 view 的 level_count 同源。
#[must_use]
pub fn full_mip_chain(width: u32, height: u32) -> u32 {
    let m = width.max(height).max(1);
    (u32::BITS - m.leading_zeros()).max(1)
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
    format: vk::Format,
    width: u32,
    height: u32,
    /// 实际建成的 mip 级数（规格的全链级数经 blit 能力降级后的值；
    /// view 的 level_count 与上传侧 blit 生成段同源）。
    mip_levels: u32,
    /// dedicated 分配的实际字节（requirements.size，显存读数口径——
    /// 分配粒度对齐后的真值，不是 width×height×bpp 的估算值）。
    alloc_bytes: u64,
}

impl GpuImage {
    /// 按契约创建:image(usage 按读回条款三用途)→ requirements → 选型(必需
    /// DEVICE_LOCAL)→ 恰量分配 → offset 0 绑定 → view。**采样器不在此建**
    ///（本体归 [`super::descriptors::BindlessTables`] 按功能键
    /// 持有——同键贴图共享一个描述符槽,槽内句柄若随某张图生灭,共享方先卸载
    /// "最后发布者"即悬空;键即完整配方,表侧 [`SamplerKey::create_sampler`] 建本体）。
    /// `sharing_families`:≥2 个不同族时按 CONCURRENT 创建(与
    /// `GpuBuffer::create_with_families` 同一取舍——EXCLUSIVE 的 release/acquire
    /// 成对语义下,验证层对"已写入但未被 draw 访问的 UAB 数组元素"的布局账本
    /// 不随 release 更新,Draw-09600 保守断言污染目标路径;内存可见性由票据信号量收口,与池同款)。
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
        sharing_families: &[u32],
    ) -> Result<Self> {
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
                "格式 {0:?} 的 optimal tiling features {features:?} 不含所需 {required_features:?},贴图无法按该使用角色使用",
                spec.format
            )));
        }
        // mip 链能力判定（5.4.3）：blit 逐级缩要求 BLIT_SRC/DST + 线性过滤
        // 位；缺任一 → 降级单级创建（RGBA8 两档本机均全含，判定留证据日志）
        let blit_ok = features.contains(
            vk::FormatFeatureFlags::BLIT_SRC
                | vk::FormatFeatureFlags::BLIT_DST
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
        );
        let mip_levels = if blit_ok { spec.mip_levels.max(1) } else { 1 };
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
                    .mip_levels(mip_levels)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    // 跨族设备:CONCURRENT 双族(transfer 写 + graphics 读,定案
                    // 见上);同族设备 EXCLUSIVE 无所有权语义
                    .sharing_mode(if sharing_families.len() >= 2 {
                        vk::SharingMode::CONCURRENT
                    } else {
                        vk::SharingMode::EXCLUSIVE
                    })
                    .queue_family_indices(sharing_families)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result =
            unsafe { Self::create_inner(device, contract, spec, usage, features, mip_levels, image) };
        // image 的回收统一收口在这里(内层只管 memory/view 的回收)
        if result.is_err() {
            unsafe { device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链的后半段(image 已建):失败路径回收已分配的 memory / 已建的 view。
    ///
    /// # Safety
    /// `image` 须是刚创建成功的合法句柄;错误返回后调用方不得再使用它。
    unsafe fn create_inner(
        device: &Device,
        contract: &MemoryContract,
        spec: &ImageSpec,
        usage: vk::ImageUsageFlags,
        features: vk::FormatFeatureFlags,
        mip_levels: u32,
        image: vk::Image,
    ) -> Result<Self> {
        unsafe {
            // 绑定三条款的证据来源:requirements 三元组,一次查询全用上。
            // dedicated 分配(显存机制篇 §6:张数少,子分配等显存压力出现再立)。
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
            // view:全量 COLOR 子资源、全 mip 链（实际级数 = 建图级数，
            // 与上传侧 blit 生成段/采样端 max_lod 同源）
            let view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(spec.format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(mip_levels)
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
            let heap = contract.heap_of_type(memory_type);
            info!(
                "贴图按契约就绪: {0:?} {1}×{2} → requirements 分配 {reqs_size}B(对齐 {reqs_alignment}B), \
                 内存类型 {memory_type} 堆 {heap_index}({heap_size}B, {heap_flags:?}), \
                 usage {usage:?} 全含于 optimal features {features:?},                  mip 链 {mip_levels} 级",
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
                format: spec.format,
                width: spec.width,
                height: spec.height,
                mip_levels,
                alloc_bytes: reqs.size,
            })
        }
    }

    /// 绑定的 Vulkan image 句柄(上传拷贝的目标、读回拷贝的源)。
    #[must_use]
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// 全量 view 句柄(描述符表从这里取)。
    #[must_use]
    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    /// 绑定的格式(证据查询口;描述符布局与此一致)。
    #[must_use]
    pub fn format(&self) -> vk::Format {
        self.format
    }

    /// 尺寸(证据查询口)。
    #[must_use]
    pub fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 实际建成的 mip 级数(上传侧 blit 生成段与 view 层级同源;1 = 单级)。
    #[must_use]
    pub fn mip_levels(&self) -> u32 {
        self.mip_levels
    }

    /// dedicated 分配字节（显存读数口径）。
    #[must_use]
    pub fn alloc_bytes(&self) -> u64 {
        self.alloc_bytes
    }
}

// # Safety:GpuImage 持有的 ash 句柄本质是整数,不指向 Rust 内存;多线程共享由
// 外部纪律保证(与 GpuBuffer 同款约定:资源创建/销毁只在持有方,方法只读句柄)
unsafe impl Send for GpuImage {}
unsafe impl Sync for GpuImage {}

impl Drop for GpuImage {
    fn drop(&mut self) {
        // 契约边界(与 GpuBuffer/Uploader 同款):不等 GPU;退出排空由 teardown_vulkan
        // 的 device_wait_idle 先于一切资源 Drop。销毁序:view 先于
        // image,memory 最后(image 销毁后再释放其绑定)。采样器本体归
        // BindlessTables,不在此销毁。
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// `bevy::image::Image` 资产身份 → 驻留票据。与 [`super::MeshPool`] 的驻留缓存同责:
/// 每帧快照带着同一批柄来,只有"查无此身份"的才创建上传(重复快照零重复上传);
/// 票据记录"这份数据何时可用"。槽位绑定由 [`super::BindlessTables`] 分配、随
/// commit 一并登记。
#[derive(bevy::prelude::Resource, Default)]
pub struct ImageCache {
    resident: HashMap<AssetId<BevyImage>, (GpuImage, Ticket, super::descriptors::SlotBinding)>,
    /// 内容替换/卸载的旧图停尸房：旧 `GpuImage`
    /// 移入此处而非就地 Drop——描述符槽改指后，在飞 draw 仍可能经旧描述符或已
    /// 取的引用读旧图，销毁必须等双条件：图形最后使用（帧槽 fence）+ 上传完成
    ///（票据——最小化期间 flush 仍可提交 transfer 而无图形提交，帧 fence 完成
    /// 不蕴含上传完成）。票据以"本图进入驻留时的批次"为准。
    graveyard: Vec<(GpuImage, Ticket)>,
}

impl ImageCache {
    /// 身份是否已驻留;在则返回其上传完成票据(消费方等待的凭据)。
    #[must_use]
    pub fn resident(&self, id: AssetId<BevyImage>) -> Option<Ticket> {
        self.resident.get(&id).map(|(_, ticket, _)| *ticket)
    }

    /// 已驻留贴图数(日志与"全部驻留"收账用)。
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    /// 停尸房当前张数(排空判定的统计口)。
    #[must_use]
    pub fn graveyard_len(&self) -> usize {
        self.graveyard.len()
    }

    /// 在场贴图 dedicated 分配字节合计(显存读数口径,5.5:requirements.size
    /// 真值求和,停尸房残图不计——回收后才出账)。
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.resident.values().map(|(image, _, _)| image.alloc_bytes()).sum()
    }

    /// 同身份内容替换登记：旧条目
    /// 原子退场、新条目随即入账。旧图进停尸房（票据随行，双条件排空见
    /// [`Self::drain_graveyard`]）；旧纹理槽进待回收账（原槽重写对在飞
    /// draw 不安全——UAB 不授权覆盖仍被未完成命令消费的描述符，改走"新槽
    /// 发布 + 旧槽延迟回收"）；旧采样器槽仅当新条目不同槽且无其余驻留共享方
    /// 才请求回收（采样器本体归表，槽退场 = 本体在排空点销毁）。未驻留身份 =
    /// 纯新登记。**必须在 [`super::descriptors::BindlessTables::publish`] 之后
    /// 调用**——新槽已定，退场判定才能对照新槽去重（同键共享则新旧同采样器
    /// 槽，不回收）。
    pub fn swap_commit(
        &mut self,
        id: AssetId<BevyImage>,
        image: GpuImage,
        ticket: Ticket,
        slots: super::descriptors::SlotBinding,
        tables: &mut super::descriptors::BindlessTables,
    ) {
        if let Some((old_image, old_ticket, old_slots)) = self.resident.remove(&id) {
            self.graveyard.push((old_image, old_ticket));
            tables.retire_texture(old_slots.texture);
            if old_slots.sampler != slots.sampler
                && !self
                    .resident
                    .values()
                    .any(|(_, _, s)| s.sampler == old_slots.sampler)
            {
                tables.retire_sampler_by_slot(old_slots.sampler);
            }
        }
        self.resident.insert(id, (image, ticket, slots));
    }

    /// 排空停尸房：双条件齐备后才销毁旧图——
    /// `wait_draws` 等"图形最后使用"（全部帧槽 fence），
    /// `uploader.wait_until` 等"上传完成"（停尸房票据的时间线信号量）。空房
    /// 直接返回不等待（稳态零成本）。
    ///
    /// # Errors
    /// 帧侧等待或票据等待失败（排空未发生，停尸房保留，下帧重试）。
    pub fn drain_graveyard(
        &mut self,
        uploader: &Uploader,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        // 票据取最大值一次等待:同一条时间线信号量按值单调,等最大即等全部
        let max_ticket = self
            .graveyard
            .iter()
            .map(|(_, ticket)| *ticket)
            .fold(0u64, u64::max);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        self.graveyard.clear(); // Drop 序：view 先于 image，memory 最后
        info!("贴图停尸房排空:销毁 {count} 张旧图（双条件齐备:图形最后使用 + 上传票据）");
        Ok(())
    }

    /// 卸载驻留条目：旧图进停尸房（票据随行）、描述符槽进待回收账
    /// ——纹理槽直接请求回收；采样器槽仅当同键最后一张退场才请求（共享去重
    /// 的反向操作，与 publish 的"同键共享"配对）。本条目出账后再查同键，检查
    /// 针对剩余驻留条目。返回是否发生退场（幂等：未驻留身份 no-op）。
    pub fn retire(
        &mut self,
        id: AssetId<BevyImage>,
        tables: &mut super::descriptors::BindlessTables,
    ) -> bool {
        let Some((image, ticket, slots)) = self.resident.remove(&id) else {
            return false;
        };
        self.graveyard.push((image, ticket));
        tables.retire_texture(slots.texture);
        if !self
            .resident
            .values()
            .any(|(_, _, s)| s.sampler == slots.sampler)
        {
            tables.retire_sampler_by_slot(slots.sampler);
        }
        true
    }

    /// 驻留登记:仅在对应批次提交成功后调用(失败路径不留账本行,与 pool.commit 同责)。
    /// 槽位绑定由 [`super::BindlessTables::publish`] 先行分配。
    pub fn commit(
        &mut self,
        id: AssetId<BevyImage>,
        image: GpuImage,
        ticket: Ticket,
        slots: super::descriptors::SlotBinding,
    ) {
        self.resident.insert(id, (image, ticket, slots));
    }

    /// 按身份取驻留贴图(绘制参数组装的取数口)。
    #[must_use]
    pub fn image(&self, id: AssetId<BevyImage>) -> Option<&GpuImage> {
        self.resident.get(&id).map(|(image, _, _)| image)
    }

    /// 按身份取槽位绑定(组装 per-draw 参数的取数口;缺资源时
    /// 消费方走 fallback 槽或暂缓 draw——`BindlessTables::fallback_slots`)。
    #[must_use]
    pub fn slots(&self, id: AssetId<BevyImage>) -> Option<super::descriptors::SlotBinding> {
        self.resident.get(&id).map(|(_, _, slots)| *slots)
    }
}

fn filter(f: ImageFilterMode) -> vk::Filter {
    match f {
        ImageFilterMode::Nearest => vk::Filter::NEAREST,
        ImageFilterMode::Linear => vk::Filter::LINEAR,
    }
}

/// 采样器功能参数键(去重定案):VkSampler 的全部功能入参——滤波两轴 +
/// mip 模式、寻址三轴、比较、边框色。两个钉号背景:
/// - **格式解释挂 image view,VkSampler 无格式参数**,所以
///   sRGB/线性色彩角色不进键——同键贴图共享采样器槽,与色彩角色无关;
/// - lod clamp 与 anisotropy 在 [`SamplerKey::create_sampler`] 里按全局策略
///   固定(max_lod=VK_LOD_CLAMP_NONE——5.4.3 起 mip 链全放开,可用层级由
///   view 级数决定;aniso off、bias 0),对全部采样器同值,不进键。
///
/// 同键 ⇒ 同一 VkSampler 配方 ⇒ 可共享描述符槽位。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SamplerKey {
    mag: i32,
    min: i32,
    mip: i32,
    u: i32,
    v: i32,
    w: i32,
    /// None = 比较关闭(与 Some(NEVER) 的 compare_enable=true 是两个功能形态)
    compare: Option<i32>,
    border: i32,
}

/// bevy 采样描述符 → [`SamplerKey`](映射与 [`SamplerKey::create_sampler`] 消费的
/// 参数一一对应,改其一必改其二)。
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
    /// 从功能键创建 VkSampler 本体（采样器归描述符表所有，键即完整
    /// 配方——raw 值即 vk 枚举）。
    /// lod 上限放开到 VK_LOD_CLAMP_NONE（5.4.3 起 mip 链全采样，可用层级由
    /// view 级数决定；单级贴图该值无害——只有 0 级可采）、anisotropy 不启用
    ///（需独立 feature，不开启）——两条全局策略随键固化。
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
            // VK_LOD_CLAMP_NONE：采样请求的 LOD 高于此值时钳到最大可用级
            //（层级由 view 决定）——单级/全链同一配方，采样器键无需携带级数
            .max_lod(vk::LOD_CLAMP_NONE)
            .compare_enable(self.compare.is_some())
            .compare_op(
                self.compare
                    .map_or(vk::CompareOp::NEVER, vk::CompareOp::from_raw),
            )
            .border_color(vk::BorderColor::from_raw(self.border))
            // anisotropy_enable false 时 clamp 值不参与采样,无需特性
            .anisotropy_enable(false);
        unsafe { device.create_sampler(&info, None) }.map_err(VulkanError::from)
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
