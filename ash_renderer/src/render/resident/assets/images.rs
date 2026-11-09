//! 贴图资源：按 [`ImageSpec`] 建 VkImage/view，按资产身份驻留缓存。格式核对在
//! [`super::image_spec`]，采样器键在 [`crate::render::resident::sampler`]。
//!
//! 三条约束（详见 `.agents/docs/开发步骤/3-静态取数链路/3.3-贴图与bindless描述符/
//! 显存机制：Buffer与Image之别、swizzle不透明与访问路径特化.md` §0）：
//! 1. OPTIMAL tiling 的 image 内容不能按字节读写：上传走 `vkCmdCopyBufferToImage`
//!    （[`crate::render::core::device::transfer`] 的图像段），读回走 `vkCmdCopyImageToBuffer`。
//! 2. 每张图一块 dedicated `VkDeviceMemory`，不进 MeshPool（贴图张数少，
//!    `maxMemoryAllocationCount` 余量充足）。
//! 3. 图像屏障要带 old/new layout 与子资源范围，transfer 里图像段单独录制，不套 buffer 屏障。
//!
//! mip：资产只取 mip0，mip 链在上传批里由 GPU blit 逐级生成（一次性，不是每帧），可用级数
//! 由 view 决定。格式不支持 blit/线性过滤时降级为单级（判定在 [`GpuImage::create`]）。

use std::collections::HashMap;

use ash::{vk, Device, Instance};
use bevy::asset::AssetId;
use bevy::image::Image as BevyImage;
use bevy::log::info;

use super::image_spec::ImageSpec;
use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::contract::MemoryContract;
use crate::render::core::device::transfer::{Ticket, Uploader};
use crate::render::resident::tables::bindless_texture_table::{BindlessTextureTable, SlotBinding};

/// 一张 2D 贴图：VkImage（OPTIMAL，UNDEFINED 起步）+ dedicated memory + 全量 view。
/// 布局迁移与上传在 [`crate::render::Uploader`] 的图像段，采样器归
/// [`BindlessTextureTable`]；本类型只管资源本体。
pub struct GpuImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// 实际建成的 mip 级数（全链级数按 blit 能力降级后的值）。
    mip_levels: u32,
    /// dedicated 分配的实际字节（requirements.size，含对齐，不是 w×h×bpp 估算）。
    alloc_bytes: u64,
}

impl GpuImage {
    /// 建 image（usage 三用途）→ 查 requirements → 选 DEVICE_LOCAL 类型 → 恰量分配 →
    /// offset 0 绑定 → 建 view。任一步失败回收已建对象。
    ///
    /// 采样器不在这里建：同参数的贴图共享一个采样器槽，若采样器随某张图生灭，先卸载的
    /// 那张会让共享方悬空；所以采样器归 [`BindlessTextureTable`] 按 [`SamplerKey`](crate::render::resident::sampler::SamplerKey) 持有。
    ///
    /// `sharing_families` 有 ≥2 个不同族时按 CONCURRENT 创建。坑：用 EXCLUSIVE + release/
    /// acquire 时，验证层对"已写入但还没被 draw 访问的 UPDATE_AFTER_BIND 数组元素"的布局
    /// 记录不随 release 更新，会误报 Draw-09600；内存可见性由上传票据信号量保证，同池。
    ///
    /// # Errors
    /// 格式的 optimal tiling feature 不足，或 Vulkan 创建/分配/绑定失败。
    pub fn create(
        device: &Device,
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        contract: &MemoryContract,
        spec: &ImageSpec,
        sharing_families: &[u32],
    ) -> Result<Self> {
        // 三用途：上传写（TRANSFER_DST）、读回校验（TRANSFER_SRC）、采样（SAMPLED）。
        // RGBA8 两档对三者都是必备支持，仍按对应的 format feature 查一遍
        let usage = vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;
        let required_features = vk::FormatFeatureFlags::TRANSFER_DST
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        let features =
            unsafe { instance.get_physical_device_format_properties(physical_device, spec.format) }
                .optimal_tiling_features;
        if !features.contains(required_features) {
            return Err(VulkanError::Init(format!(
                "格式 {0:?} 的 optimal tiling features {features:?} 不含所需 {required_features:?},贴图无法按该使用角色使用",
                spec.format
            )));
        }
        // blit 逐级缩小要 BLIT_SRC/DST + 线性过滤，缺任一降为单级
        let blit_ok = features.contains(
            vk::FormatFeatureFlags::BLIT_SRC
                | vk::FormatFeatureFlags::BLIT_DST
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
        );
        let mip_levels = if blit_ok { spec.mip_levels.max(1) } else { 1 };
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
        let result = unsafe {
            Self::create_inner(device, contract, spec, usage, features, mip_levels, image)
        };
        // image 统一在这里回收，内层只管 memory/view
        if result.is_err() {
            unsafe { device.destroy_image(image, None) };
        }
        result
    }

    /// 创建的后半段（image 已建）：失败时回收已分配的 memory。
    ///
    /// # Safety
    /// `image` 须是刚创建成功的合法句柄；错误返回后调用方不得再使用它。
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
            device
                .bind_image_memory(image, memory, 0)
                .inspect_err(|_| device.free_memory(memory, None))?;
            // 全量 COLOR 子资源、全部实际 mip 级
            let view = device
                .create_image_view(
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
                )
                .inspect_err(|_| device.free_memory(memory, None))?;
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
                mip_levels,
                alloc_bytes: reqs.size,
            })
        }
    }

    /// image 句柄（上传拷贝的目标、读回拷贝的源）。
    #[must_use]
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// 全量 view 句柄（发布进描述符表）。
    #[must_use]
    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    /// 实际建成的 mip 级数（1 = 单级）。
    #[must_use]
    pub fn mip_levels(&self) -> u32 {
        self.mip_levels
    }

    #[must_use]
    pub fn alloc_bytes(&self) -> u64 {
        self.alloc_bytes
    }
}

// # Safety：ash 句柄本质是整数，不指向 Rust 内存；创建/销毁只在持有方，方法只读句柄（同 GpuBuffer）
unsafe impl Send for GpuImage {}
unsafe impl Sync for GpuImage {}

impl Drop for GpuImage {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。view → image → memory。
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 贴图驻留缓存：资产身份 → (贴图, 上传票据, 描述符槽)。只有查不到的身份才创建上传；
/// 槽位由 [`BindlessTextureTable::publish`] 先分配，随 commit 一并登记。
#[derive(bevy::prelude::Resource, Default)]
pub struct ImageCache {
    resident: HashMap<AssetId<BevyImage>, (GpuImage, Ticket, SlotBinding)>,
    /// 替换/卸载换下的旧图（连同其入驻时的上传票据）：描述符槽改指后，在飞 draw 仍可能
    /// 读旧图，等 [`Self::drain_graveyard`] 的两个条件满足才销毁。
    graveyard: Vec<(GpuImage, Ticket)>,
}

impl ImageCache {
    /// 已驻留则返回其上传票据。
    #[must_use]
    pub fn resident(&self, id: AssetId<BevyImage>) -> Option<Ticket> {
        self.resident.get(&id).map(|(_, ticket, _)| *ticket)
    }

    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    /// 待销毁的旧图张数。
    #[must_use]
    pub fn graveyard_len(&self) -> usize {
        self.graveyard.len()
    }

    /// 驻留贴图的分配字节合计（不含待销毁的旧图）。
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.resident
            .values()
            .map(|(image, _, _)| image.alloc_bytes())
            .sum()
    }

    /// 同身份内容替换：旧图进待销毁账，旧槽位交回表（不原地重写槽——在飞 draw 还在读，
    /// 改为"新槽发布 + 旧槽延迟回收"）。未驻留的身份等同 commit。
    /// **必须在 [`BindlessTextureTable::publish`] 之后调用**：先发布再交回，同参数采样器的
    /// 引用数才不会中途归零。
    pub fn swap_commit(
        &mut self,
        id: AssetId<BevyImage>,
        image: GpuImage,
        ticket: Ticket,
        slots: SlotBinding,
        tables: &mut BindlessTextureTable,
    ) {
        self.retire(id, tables);
        self.resident.insert(id, (image, ticket, slots));
    }

    /// 两个条件都满足才销毁旧图：`wait_draws` 等全部帧槽 fence，`uploader` 等旧图的上传
    /// 票据。没有旧图时不等待。
    ///
    /// 坑：帧 fence 不代表上传完成——窗口最小化时 flush 仍会提交 transfer 而没有图形提交。
    ///
    /// # Errors
    /// 等待失败（旧图保留，下帧重试）。
    pub fn drain_graveyard(
        &mut self,
        uploader: &Uploader,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        // 同一条时间线信号量按值单调，等最大票据即等全部
        let max_ticket = self
            .graveyard
            .iter()
            .map(|(_, ticket)| *ticket)
            .max()
            .unwrap_or(0);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        self.graveyard.clear();
        info!("贴图待回收队列排空:销毁 {count} 张旧图（双条件齐备:图形最后使用 + 上传票据）");
        Ok(())
    }

    /// 卸载：旧图进待销毁账，槽位交回表（采样器是否回收由表按引用数决定）。
    /// 返回是否真的卸载了（未驻留返回 false）。
    pub fn retire(&mut self, id: AssetId<BevyImage>, tables: &mut BindlessTextureTable) -> bool {
        let Some((image, ticket, slots)) = self.resident.remove(&id) else {
            return false;
        };
        self.graveyard.push((image, ticket));
        tables.retire(slots);
        true
    }

    /// 登记驻留：仅在对应批次提交成功后调用。
    pub fn commit(
        &mut self,
        id: AssetId<BevyImage>,
        image: GpuImage,
        ticket: Ticket,
        slots: SlotBinding,
    ) {
        self.resident.insert(id, (image, ticket, slots));
    }

    #[must_use]
    pub fn image(&self, id: AssetId<BevyImage>) -> Option<&GpuImage> {
        self.resident.get(&id).map(|(image, _, _)| image)
    }

    /// 按身份取槽位；None 时调用方用 [`BindlessTextureTable::fallback_slots`]。
    #[must_use]
    pub fn slots(&self, id: AssetId<BevyImage>) -> Option<SlotBinding> {
        self.resident.get(&id).map(|(_, _, slots)| *slots)
    }
}
