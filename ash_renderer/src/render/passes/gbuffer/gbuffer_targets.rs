//! G-buffer 与 HDR 中间目标：不透明物体写入的附件，加合成与透明层共用的 HDR
//! 落色目标；每帧槽一套，随 swapchain 尺寸重建。
//!
//! 附件布局（逻辑槽号固定，后续插入不改既有槽号；槽 2 预留给 motion vectors，
//! 不分配，所以 matparam 的颜色附件下标是 2）：
//!
//! | 槽 | 附件下标 | 附件 | 格式 | 语义 |
//! |---|---|---|---|---|
//! | 0 | 0 | albedo | R8G8B8A8_UNORM | 线性反照率（贴图采样 × base_color，sRGB 解码在采样端） |
//! | 1 | 1 | normal | R16G16B16A16_SFLOAT | 世界空间法线 xyz 直存（w 未用） |
//! | 2 | — | motion vectors | 不分配 | 预留 |
//! | 3 | 2 | matparam | R8G8B8A8_UNORM | 材质标量：r=metallic / g=AO / b=perceptualRoughness / a=reflectance |
//! | — | 深度 | depth | D32_SFLOAT | reverse-Z 深度（本体按帧槽在 [`crate::render::core::target::depth`]） |
//!
//! **HDR 中间目标**（R16G16B16A16_SFLOAT）：合成与透明前向层写入，present tonemap
//! 从它采样。透明层要在它上面混合，建目标时查证格式支持混合（[`ColorTarget::new`]）。
//!
//! 采样集（[`GBufferTargets::sampled_set`]）把附件视图按只读布局发布：合成读
//! albedo/normal/matparam/depth，Bloom prefilter 与 present tonemap 读 hdr。深度视图
//! 归 FramePool，本结构须先于 FramePool 销毁（拆除序保证）。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::core::target::color::{ColorTarget, HDR_FORMAT};
use crate::render::Context;

/// albedo 附件格式：存采样时已解码的线性反照率；sRGB 编码只在最终写 swapchain 时做一次。
pub const ALBEDO_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// normal 附件格式：世界空间法线 xyz 直存，half 精度。
pub const NORMAL_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// matparam 附件格式：r=metallic / g=AO / b=perceptualRoughness / a=reflectance，
/// 这些标量 8bit 精度足够。
pub const MATPARAM_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// 采样集 binding 编号（合成、Bloom prefilter 与 present tonemap 共用同一布局）。
const SAMPLED_BINDING_ALBEDO: u32 = 0;
const SAMPLED_BINDING_NORMAL: u32 = 1;
const SAMPLED_BINDING_HDR: u32 = 2;
const SAMPLED_BINDING_DEPTH: u32 = 3;
const SAMPLED_BINDING_SAMPLER: u32 = 4;
const SAMPLED_BINDING_MATPARAM: u32 = 5;

const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// G-buffer 采样集：五张附件视图 + 一个共享采样器，片元阶段读。
pub(crate) static GBUFFER_SAMPLED_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "gbuffer_sampled",
    &[
        BindingDesc::new(SAMPLED_BINDING_ALBEDO, DescKind::SampledImage, FS),
        BindingDesc::new(SAMPLED_BINDING_NORMAL, DescKind::SampledImage, FS),
        BindingDesc::new(SAMPLED_BINDING_HDR, DescKind::SampledImage, FS),
        BindingDesc::new(SAMPLED_BINDING_DEPTH, DescKind::SampledImage, FS),
        BindingDesc::new(SAMPLED_BINDING_MATPARAM, DescKind::SampledImage, FS),
        BindingDesc::new(SAMPLED_BINDING_SAMPLER, DescKind::Sampler, FS),
    ],
);

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
    /// 采样集布局（合成、Bloom prefilter 与 present tonemap 管线的 layout 素材）。
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 采样集，与帧槽一一对应。
    sets: Vec<vk::DescriptorSet>,
    /// 附件采样共用的线性采样器（clamp-to-edge）。
    sampler: vk::Sampler,
}

impl GBufferTargets {
    /// 每帧槽建 albedo/normal/matparam/hdr 四张目标并写好采样集。深度视图由调用
    /// 方传（本体归 FramePool）。
    ///
    /// # Errors
    /// 格式 feature 不足、Vulkan 创建/分配失败、描述符集分配/写入失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D, depth_views: &[vk::ImageView]) -> Result<Self> {
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

    /// 采样集布局（登记给管线组装）。
    #[must_use]
    pub fn sampled_layout(&self) -> SetLayout {
        SetLayout::new(&GBUFFER_SAMPLED_SET_DESC, self.set_layout)
    }

    /// 第 `slot` 帧槽的采样集。
    #[must_use]
    pub fn sampled_set(&self, slot: usize) -> DescSet {
        DescSet::new(&GBUFFER_SAMPLED_SET_DESC, self.sets[slot])
    }

    /// 第 `slot` 帧槽的 albedo 附件视图（G-buffer pass 颜色附件 0）。
    #[must_use]
    pub fn albedo_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].albedo.view()
    }

    /// 第 `slot` 帧槽的 normal 附件视图（颜色附件 1）。
    #[must_use]
    pub fn normal_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].normal.view()
    }

    /// 第 `slot` 帧槽的 matparam 附件视图（颜色附件 2）。
    #[must_use]
    pub fn matparam_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].matparam.view()
    }

    /// 第 `slot` 帧槽的 HDR 中间目标视图（合成/透明层颜色附件 0）。
    #[must_use]
    pub fn hdr_view(&self, slot: usize) -> vk::ImageView {
        self.slots[slot].hdr.view()
    }

    /// 第 `slot` 帧槽的 albedo image 句柄（布局屏障用）。
    #[must_use]
    pub fn albedo_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].albedo.image()
    }

    /// 第 `slot` 帧槽的 normal image 句柄（同上）。
    #[must_use]
    pub fn normal_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].normal.image()
    }

    /// 第 `slot` 帧槽的 matparam image 句柄（同上）。
    #[must_use]
    pub fn matparam_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].matparam.image()
    }

    /// 第 `slot` 帧槽的 HDR image 句柄（同上）。
    #[must_use]
    pub fn hdr_image(&self, slot: usize) -> vk::Image {
        self.slots[slot].hdr.image()
    }

    /// 随 swapchain 尺寸重建（extent 没变直接返回）。销毁旧资源前自带
    /// device_wait_idle；深度视图须已按新尺寸重建（rebuild_depth 先行）。
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
            unsafe {
                self.device
                    .destroy_descriptor_set_layout(self.set_layout, None)
            };
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
        // 不等 GPU：重建路径的等待在 rebuild，退出排空在 teardown
        self.drop_resources();
    }
}

/// 采样集建链：布局 + pool + 每帧槽一集 + 共享线性采样器。半建失败逐层回收（全有或全无）。
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
    let set_layout = GBUFFER_SAMPLED_SET_DESC.create(device)?;
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
        let pool_sizes = GBUFFER_SAMPLED_SET_DESC.pool_sizes_vec(depth_views.len() as u32);
        let pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(depth_views.len() as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        let sets = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(
                    &std::iter::repeat_n(set_layout, depth_views.len()).collect::<Vec<_>>(),
                ),
        );
        let sets = match sets {
            Ok(s) => s,
            Err(e) => {
                device.destroy_descriptor_pool(pool, None);
                return Err(VulkanError::Vk(e));
            }
        };
        // mipmap 不参与，NEAREST 即可
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
        // 写入按 GBUFFER_SAMPLED_SET_DESC 核对种类，写满才提交
        for ((set, slot), depth_view) in sets.iter().zip(slots).zip(depth_views) {
            let mut writer = DescriptorWriter::new(&GBUFFER_SAMPLED_SET_DESC, *set);
            writer
                .image(
                    SAMPLED_BINDING_ALBEDO,
                    slot.albedo.view(),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                )
                .image(
                    SAMPLED_BINDING_NORMAL,
                    slot.normal.view(),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                )
                .image(
                    SAMPLED_BINDING_HDR,
                    slot.hdr.view(),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                )
                // 坑：深度布局须与帧编排转只读的目标布局一致（DEPTH_STENCIL_READ_ONLY，
                // 不是 SHADER_READ_ONLY），否则 VUID-00344
                .image(
                    SAMPLED_BINDING_DEPTH,
                    *depth_view,
                    vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
                )
                .image(
                    SAMPLED_BINDING_MATPARAM,
                    slot.matparam.view(),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                )
                .sampler(SAMPLED_BINDING_SAMPLER, sampler);
            writer.submit(device)?;
        }
        info!(
            "G-buffer 采样集就绪: b0 albedo / b1 normal / b2 hdr / b3 depth / b4 线性采样器 / b5 matparam × {} 帧槽（{}x{}）",
            depth_views.len(),
            extent.width,
            extent.height,
        );
        Ok((set_layout, pool, sets, sampler))
    }
}
