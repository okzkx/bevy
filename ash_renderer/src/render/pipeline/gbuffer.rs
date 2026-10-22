//! G-buffer 与中间渲染目标（8.1）：不透明 deferred 写入的附件面 + 合成/透明
//! 层共用的中间颜色目标，随帧槽/swapchain 尺寸重建。
//!
//! 附件布局契约（槽位 = 渲染实例的 color attachment 顺序；motion vectors
//! 只留位不分配——本体归步骤 9.x 前置施工件，需 InstanceLedger prev/curr
//! 双份模型环；槽位号在此钉死，后续插入不动既有槽位语义）：
//!
//! | 槽 | 附件 | 格式 | 语义 |
//! |---|---|---|---|
//! | 0 | albedo | R8G8B8A8_UNORM | 线性反照率（贴图采样 × base_color，sRGB 解码在采样端） |
//! | 1 | normal | R16G16B16A16_SFLOAT | 世界空间法线 xyz 直存（w 未用；编码取舍见施工记录） |
//! | 2 | motion vectors | ——（不分配） | 留位：槽位号保留 |
//! | 3 | matparam | R8G8B8A8_UNORM | 材质标量（8.2.1）：r=metallic / g=AO / b=perceptualRoughness / a=reflectance（8.2.3 转正）——光照累积的 PBR 入参 |
//! | — | depth | D32_SFLOAT | reverse-Z 深度（本体按帧槽在 [`super::depth`]） |
//!
//! **HDR 中间目标 hdr**（R16G16B16A16_SFLOAT）：合成（G-buffer 光照）与透明
//! forward 层的落色面——HDR 契约留位一次到位（8.4 正式启用 Bloom/ToneMapping）；
//! present blit 从它采样写 swapchain（SRGB view 的 ROP 编码恰好一次）。
//! 该格式的 blending 支持是步骤 8 前置闸门的实测项——[`ColorTarget::new`]
//! 逐格式查证落日志（支持≠启用纪律）。
//!
//! 采样集（[`Self::sampled_set`]）把附件视图以 SHADER_READ_ONLY 布局发布：
//! 合成 pass 读 albedo/normal/depth，present blit 读 hdr。视图本体寿命令归
//! 本结构，"视图先于附件销毁"由拆除序（本结构先于 FramePool）保证。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::Context;

/// albedo 附件格式（线性 8bit——G-buffer 存采样端已解码的线性反照率，
/// 编码恰好一次判定线不受影响：SRGB 编码仍只在最终 ROP 写出发生一次）。
pub const ALBEDO_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// normal 附件格式（世界空间 xyz 直存，half 精度足够；八面体压缩的带宽账
/// 留作后续优化项，不主张）。
pub const NORMAL_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// matparam 附件格式（8.2.1）：材质标量 r=metallic / g=AO / b=perceptualRoughness
/// / a=reflectance。8bit 量化与 Filament deferred 层同档（roughness/metallic
/// 均走 8bit 打包），PBR 语义下精度足够；法线/反照率的编码取舍不适用于标量面。
pub const MATPARAM_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// HDR 中间目标格式（透明层的 alpha blending 也落在此目标上）。
pub const HDR_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// 采样集 binding 编号（合成 pass 与 present blit 共用同一布局）。
pub const SAMPLED_BINDING_ALBEDO: u32 = 0;
pub const SAMPLED_BINDING_NORMAL: u32 = 1;
pub const SAMPLED_BINDING_HDR: u32 = 2;
pub const SAMPLED_BINDING_DEPTH: u32 = 3;
pub const SAMPLED_BINDING_SAMPLER: u32 = 4;
pub const SAMPLED_BINDING_MATPARAM: u32 = 5;

/// 一张颜色附件：VkImage + dedicated memory + view（COLOR_ATTACHMENT |
/// SAMPLED 用途——G-buffer 附件渲染后由合成/blit 采样）。
struct ColorTarget {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl ColorTarget {
    /// 按契约创建颜色附件：格式 feature 逐位查证（支持与启用分开），全有或全无。
    ///
    /// # Errors
    /// 格式 feature 不足或 Vulkan 创建/分配/绑定失败。
    fn new(
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
        // HDR 契约前置闸门（步骤级计划 §1 8.1）：中间目标要接透明层 blending
        //——支持位实测落日志（该格式 blending 核心规范 mandatory，仍不省查询）
        if need_blend && !features.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND) {
            return Err(VulkanError::Init(format!(
                "{role} 格式 {format:?} 不支持 COLOR_ATTACHMENT_BLEND，透明层 blending 前置闸门不通过"
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
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(ctx, image, format) };
        if result.is_err() {
            unsafe { ctx.device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链后半段（image 已建）：失败路径回收 memory/view。
    ///
    /// # Safety
    /// `image` 须刚创建成功且未销毁；错误返回后调用方不得再使用它。
    unsafe fn create_inner(ctx: &Context, image: vk::Image, format: vk::Format) -> Result<Self> {
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
            // view:COLOR aspect、mip0 单层
            let view = ctx.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
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
                    ctx.device.free_memory(memory, None);
                    return Err(VulkanError::Vk(e));
                }
            };
            Ok(Self {
                device: ctx.device.clone(),
                image,
                memory,
                view,
            })
        }
    }
}

impl Drop for ColorTarget {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——重建路径的等待在 rebuild，退出排空在 teardown
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 每帧槽一份的 G-buffer 附件（在飞两帧并发写各自槽位）。
struct GBufferSlot {
    albedo: ColorTarget,
    normal: ColorTarget,
    matparam: ColorTarget,
    hdr: ColorTarget,
}

/// G-buffer 附件 + HDR 中间目标（×`MAX_FRAMES_IN_FLIGHT` 帧槽）+ 采样描述符集。
#[derive(bevy::prelude::Resource)]
pub struct GBufferTargets {
    device: Device,
    extent: vk::Extent2D,
    slots: Vec<GBufferSlot>,
    /// 采样集布局（合成 pass 与 present blit 管线的 layout 素材）。
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 采样集：帧槽一一对应（b0 albedo / b1 normal / b2 hdr / b3 depth / b4 采样器）。
    sets: Vec<vk::DescriptorSet>,
    /// 附件采样的共享线性采样器（clamp-to-edge；G-buffer 采样无 UV 出界需求）。
    sampler: vk::Sampler,
}

impl GBufferTargets {
    /// 建 G-buffer 附件与采样集：逐格式查证落日志 → 每帧槽 albedo/normal/hdr
    /// 三张附件 → 采样集写入（视图 + SHADER_READ_ONLY 布局）。深度视图由调用
    /// 方传（FramePool 持有本体）。
    ///
    /// # Errors
    /// 格式 feature 不足、Vulkan 创建/分配失败、描述符集分配/写入失败。
    pub fn new(
        ctx: &Context,
        extent: vk::Extent2D,
        depth_views: &[vk::ImageView],
    ) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            extent: vk::Extent2D::default(),
            slots: Vec::new(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            sampler: vk::Sampler::null(),
        };
        this.build(ctx, extent, depth_views)?;
        info!(
            "G-buffer 附件就绪: albedo {ALBEDO_FORMAT:?} + normal {NORMAL_FORMAT:?} + matparam {MATPARAM_FORMAT:?} + HDR {HDR_FORMAT:?} \
             × {} 帧槽（motion vectors 留位不分配，槽位契约见模块注释）",
            depth_views.len(),
        );
        Ok(this)
    }

    /// 采样集布局（合成/blit 管线 layout 的素材；集与布局同源纪律）。
    #[must_use]
    pub fn sampled_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    /// 第 `slot` 帧槽的采样集（合成 pass 与 present blit 绑定）。
    #[must_use]
    pub fn sampled_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot]
    }

    /// 第 `slot` 帧槽的 albedo 附件视图（G-buffer pass attachment 0）。
    #[must_use]
    pub fn albedo_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].albedo.view
    }

    /// 第 `slot` 帧槽的 normal 附件视图（attachment 1）。
    #[must_use]
    pub fn normal_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].normal.view
    }

    /// 第 `slot` 帧槽的 matparam 附件视图（attachment 2，槽位号 3——motion
    /// vectors 留位不动，8.2.1）。
    #[must_use]
    pub fn matparam_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].matparam.view
    }

    /// 第 `slot` 帧槽的 HDR 中间目标视图（合成/透明层 attachment 0）。
    #[must_use]
    pub fn hdr_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].hdr.view
    }

    /// 第 `slot` 帧槽的 albedo image 句柄（进场/转只读屏障的目标，8.1）。
    #[must_use]
    pub fn albedo_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].albedo.image
    }

    /// 第 `slot` 帧槽的 normal image 句柄（同上）。
    #[must_use]
    pub fn normal_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].normal.image
    }

    /// 第 `slot` 帧槽的 matparam image 句柄（同上，8.2.1）。
    #[must_use]
    pub fn matparam_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].matparam.image
    }

    /// 第 `slot` 帧槽的 HDR image 句柄（同上）。
    #[must_use]
    pub fn hdr_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].hdr.image
    }

    /// 当前尺寸（resize 幂等判据）。
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 随 swapchain 尺寸重建（幂等：extent 没变就空手而归）。与深度附件同
    /// 纪律：销毁旧资源前自带 device_wait_idle；深度视图须已按新尺寸重建
    ///（rebuild_depth 先行）。
    ///
    /// # Errors
    /// 等待排空或新附件创建失败。
    pub fn rebuild(
        &mut self,
        ctx: &Context,
        extent: vk::Extent2D,
        depth_views: &[vk::ImageView],
    ) -> Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        self.drop_resources();
        self.build(ctx, extent, depth_views)?;
        info!(
            "G-buffer 附件随 swapchain 重建: {}x{} × {} 帧槽",
            extent.width,
            extent.height,
            depth_views.len(),
        );
        Ok(())
    }

    fn build(
        &mut self,
        ctx: &Context,
        extent: vk::Extent2D,
        depth_views: &[vk::ImageView],
    ) -> Result<()> {
        let slots = (0..depth_views.len())
            .map(|_| {
                Ok(GBufferSlot {
                    albedo: ColorTarget::new(ctx, extent, ALBEDO_FORMAT, false, "G-buffer albedo")?,
                    normal: ColorTarget::new(ctx, extent, NORMAL_FORMAT, false, "G-buffer normal")?,
                    matparam: ColorTarget::new(
                        ctx,
                        extent,
                        MATPARAM_FORMAT,
                        false,
                        "G-buffer matparam",
                    )?,
                    hdr: ColorTarget::new(ctx, extent, HDR_FORMAT, true, "HDR 中间目标")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (set_layout, pool, sets, sampler) = build_sampling(ctx, extent, &slots, depth_views)?;
        self.extent = extent;
        self.slots = slots;
        self.set_layout = set_layout;
        self.pool = pool;
        self.sets = sets;
        self.sampler = sampler;
        Ok(())
    }

    fn drop_resources(&mut self) {
        self.slots.clear();
        self.sets.clear();
        if self.pool != vk::DescriptorPool::null() {
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
            self.pool = vk::DescriptorPool::null();
        }
        if self.set_layout != vk::DescriptorSetLayout::null() {
            unsafe { self.device.destroy_descriptor_set_layout(self.set_layout, None) };
            self.set_layout = vk::DescriptorSetLayout::null();
        }
        if self.sampler != vk::Sampler::null() {
            unsafe { self.device.destroy_sampler(self.sampler, None) };
            self.sampler = vk::Sampler::null();
        }
    }
}

impl Drop for GBufferTargets {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——重建路径的等待在 rebuild，退出排空在 teardown
        //（拆除序：本结构先于 FramePool，采样集里的深度视图先于深度本体销毁）
        self.drop_resources();
    }
}

/// 采样集建链：布局（b0..b4）+ pool + 每帧槽一集（视图 + SHADER_READ_ONLY
/// 布局写入）+ 共享线性采样器。半建失败逐层回收（全有或全无）。
fn build_sampling(
    ctx: &Context,
    extent: vk::Extent2D,
    slots: &[GBufferSlot],
    depth_views: &[vk::ImageView],
) -> Result<(
    vk::DescriptorSetLayout,
    vk::DescriptorPool,
    Vec<vk::DescriptorSet>,
    vk::Sampler,
)> {
    let device = &ctx.device;
    let stages = vk::ShaderStageFlags::FRAGMENT;
    let sampled = |binding: u32| {
        vk::DescriptorSetLayoutBinding::default()
            .binding(binding)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(1)
            .stage_flags(stages)
    };
    let bindings = [
        sampled(SAMPLED_BINDING_ALBEDO),
        sampled(SAMPLED_BINDING_NORMAL),
        sampled(SAMPLED_BINDING_HDR),
        sampled(SAMPLED_BINDING_DEPTH),
        sampled(SAMPLED_BINDING_MATPARAM),
        vk::DescriptorSetLayoutBinding::default()
            .binding(SAMPLED_BINDING_SAMPLER)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .descriptor_count(1)
            .stage_flags(stages),
    ];
    let set_layout = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )
    }?;
    let result = unsafe { build_sampling_inner(ctx, extent, slots, depth_views, set_layout) };
    if result.is_err() {
        unsafe { device.destroy_descriptor_set_layout(set_layout, None) };
    }
    result
}

/// 采样集建链后半段（布局已建）：失败路径回收 pool/sets/sampler。
///
/// # Safety
/// `set_layout` 须刚创建成功且未销毁。
unsafe fn build_sampling_inner(
    ctx: &Context,
    extent: vk::Extent2D,
    slots: &[GBufferSlot],
    depth_views: &[vk::ImageView],
    set_layout: vk::DescriptorSetLayout,
) -> Result<(
    vk::DescriptorSetLayout,
    vk::DescriptorPool,
    Vec<vk::DescriptorSet>,
    vk::Sampler,
)> {
    unsafe {
        let device = &ctx.device;
        let pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(depth_views.len() as u32)
                .pool_sizes(&[
                    vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::SAMPLED_IMAGE)
                        .descriptor_count(5 * depth_views.len() as u32),
                    vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::SAMPLER)
                        .descriptor_count(depth_views.len() as u32),
                ]),
            None,
        )?;
        let sets = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&std::iter::repeat_n(set_layout, depth_views.len()).collect::<Vec<_>>()),
        );
        let sets = match sets {
            Ok(s) => s,
            Err(e) => {
                device.destroy_descriptor_pool(pool, None);
                return Err(VulkanError::Vk(e));
            }
        };
        // 共享线性采样器（clamp-to-edge 保底；mipmap 不参与，NEAREST 即可）
        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        );
        let sampler = match sampler {
            Ok(s) => s,
            Err(e) => {
                device.destroy_descriptor_pool(pool, None);
                return Err(VulkanError::from(e));
            }
        };
        // 先收集所有 image_info（ owned，WriteDescriptorSet 借用它们——
        // 集合生命周期覆盖 update_descriptor_sets 调用）
        let mut infos: Vec<Vec<vk::DescriptorImageInfo>> = Vec::with_capacity(sets.len());
        for ((_, slot), depth_view) in sets.iter().zip(slots).zip(depth_views) {
            let image_info = |view: vk::ImageView| {
                vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            };
            infos.push(vec![
                image_info(slot.albedo.view),
                image_info(slot.normal.view),
                image_info(slot.hdr.view),
                // 深度布局与帧编排的转只读目标一致（VUID-00344：描述符布局
                // 须与 draw 时实际布局一致——DEPTH_READ_ONLY 非 SHADER_READ_ONLY）
                vk::DescriptorImageInfo::default()
                    .image_view(*depth_view)
                    .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL),
                image_info(slot.matparam.view),
                vk::DescriptorImageInfo::default().sampler(sampler),
            ]);
        }
        let bindings = [
            SAMPLED_BINDING_ALBEDO,
            SAMPLED_BINDING_NORMAL,
            SAMPLED_BINDING_HDR,
            SAMPLED_BINDING_DEPTH,
            SAMPLED_BINDING_MATPARAM,
            SAMPLED_BINDING_SAMPLER,
        ];
        let writes: Vec<vk::WriteDescriptorSet> = sets
            .iter()
            .zip(&infos)
            .flat_map(|(set, infos)| {
                bindings.iter().zip(infos).map(|(binding, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(*binding)
                        .descriptor_type(if *binding == SAMPLED_BINDING_SAMPLER {
                            vk::DescriptorType::SAMPLER
                        } else {
                            vk::DescriptorType::SAMPLED_IMAGE
                        })
                        .image_info(std::slice::from_ref(info))
                })
            })
            .collect();
        device.update_descriptor_sets(&writes, &[]);
        info!(
            "G-buffer 采样集就绪: b0 albedo / b1 normal / b2 hdr / b3 depth / b4 线性采样器 / b5 matparam × {} 帧槽（{}x{}）",
            depth_views.len(),
            extent.width,
            extent.height,
        );
        Ok((set_layout, pool, sets, sampler))
    }
}
