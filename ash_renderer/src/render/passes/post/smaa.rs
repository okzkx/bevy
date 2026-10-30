//! 8.5 SMAA 资源族（官方 1x 三 pass）：LDR 中间目标（tonemap 落点）+ edges/
//! blend 中间目标 + 两张预生成 LUT（area 160×560 R8G8 / search 64×16 R8，
//! 源 = bevy_anti_alias 内置 ktx2 转档）+ SmaaInfo UBO 帧槽环 + 描述符。
//!
//! 链路：composite HDR → tonemap（渲到 LDR UNORM 目标——线性域，UNORM 仅
//! clamp）→ edge 检测（读 LDR）→ blend 权重（读 edges + LUT）→ neighborhood
//! 混合（读 LDR + blend → swapchain，SRGB view ROP 编码恰好一次）。三 pass
//! 中间目标 UNORM 存储 + 线性采样 = 线性域抗锯齿（官方"非 sRGB 读写"条款
//! 的线性管线实现：全链无编码，编码仍在终点 view 恰好一次）。
//!
//! 关闭路径：smaa_set off = edge/blend 跳过、neighborhood 直通（shader 内
//! 旁路），tonemap → neighborhood → swapchain 仍走全链（画面回 8.4 基线）。
//! 目标布局纪律照 bloom：进出屏障恒录（UNDEFINED 进 = 内容可弃）。

use ash::{vk, Device};
use bevy::image::{CompressedImageFormats, Image, ImageType};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::resident::images::GpuImage;
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

use crate::render::core::target::color::ColorTarget;
use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::pipeline::graphics::PipelineRef;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{merged_pool_sizes, stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts};
use crate::render::core::binding::shader_abi;

const SMAA_AREA_LUT_PNG: &[u8] = include_bytes!("../../../../shaders/_smaa_area_lut.png");
const SMAA_SEARCH_LUT_PNG: &[u8] = include_bytes!("../../../../shaders/_smaa_search_lut.png");

pub const SMAA_BINDING_TEX0: u32 = 0;
pub const SMAA_BINDING_INFO: u32 = 1;
pub const SMAA_BINDING_SAMPLER: u32 = 2;
pub const SMAA_BINDING_TEX1: u32 = 3;
pub const SMAA_BINDING_TEX2: u32 = 4;

pub(crate) const SMAA_INFO_SIZE: u32 = 32;

const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;
const VS_FS: vk::ShaderStageFlags = stages(&[vk::ShaderStageFlags::VERTEX, FS]);

/// edge/blend 共用：b0 主纹理 / b1 参数 UBO（VS 也读）/ b2 采样器 / b3、b4 两张辅助纹理。
/// b3/b4 只有 blend 集写（search/area 查找表），edge 集不写不读——标 PARTIALLY_BOUND
/// 免除写满要求，shader 侧也只有 blend 声明并访问它们。
pub(crate) static SMAA_A_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "smaa_a",
    &[
        BindingDesc::new(SMAA_BINDING_TEX0, DescKind::SampledImage, FS),
        BindingDesc::new(SMAA_BINDING_INFO, DescKind::UniformBuffer, VS_FS),
        BindingDesc::new(SMAA_BINDING_SAMPLER, DescKind::Sampler, FS),
        BindingDesc::new(SMAA_BINDING_TEX1, DescKind::SampledImage, FS)
            .array(1, vk::DescriptorBindingFlags::PARTIALLY_BOUND),
        BindingDesc::new(SMAA_BINDING_TEX2, DescKind::SampledImage, FS)
            .array(1, vk::DescriptorBindingFlags::PARTIALLY_BOUND),
    ],
);

/// neighborhood：同 A，但 b4 换成第二个采样器。
pub(crate) static SMAA_B_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "smaa_b",
    &[
        BindingDesc::new(SMAA_BINDING_TEX0, DescKind::SampledImage, FS),
        BindingDesc::new(SMAA_BINDING_INFO, DescKind::UniformBuffer, VS_FS),
        BindingDesc::new(SMAA_BINDING_SAMPLER, DescKind::Sampler, FS),
        BindingDesc::new(SMAA_BINDING_TEX1, DescKind::SampledImage, FS),
        BindingDesc::new(SMAA_BINDING_TEX2, DescKind::Sampler, FS),
    ],
);

pub(crate) static SMAA_EDGE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "smaa_edge",
    shader: &shader_abi::SMAA_EDGE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&SMAA_A_SET_DESC],
    push: None,
    color_formats: &[vk::Format::R8G8B8A8_UNORM],
    depth_format: None,
};

pub(crate) static SMAA_BLEND_PIPELINE: PipelineDesc = PipelineDesc {
    label: "smaa_blend",
    shader: &shader_abi::SMAA_BLEND,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&SMAA_A_SET_DESC],
    push: None,
    color_formats: &[vk::Format::R8G8B8A8_UNORM],
    depth_format: None,
};

pub(crate) static SMAA_NEIGHBORHOOD_PIPELINE: PipelineDesc = PipelineDesc {
    label: "smaa_neighborhood",
    shader: &shader_abi::SMAA_NEIGHBORHOOD,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&SMAA_B_SET_DESC],
    push: None,
    color_formats: &[crate::render::SWAPCHAIN_COLOR_FORMAT],
    depth_format: None,
};

/// SMAA 开关（BRP `smaa_set` 写，draw_frame 读）。
#[derive(Resource)]
pub struct SmaaSettings {
    pub enabled: bool,
}

impl Default for SmaaSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// SMAA 资源族。
#[derive(Resource)]
pub struct SmaaTargets {
    device: Device,
    set_layout_a: vk::DescriptorSetLayout,
    set_layout_b: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets_edge: Vec<vk::DescriptorSet>,
    sets_blend: Vec<vk::DescriptorSet>,
    sets_nb: Vec<vk::DescriptorSet>,
    /// LDR/edges/blend 中间目标 ×帧槽环（两帧在飞各用各的——单份会被
    /// 在飞帧布局竞争，WRITE_AFTER_WRITE 实抓即修）。
    ldrs: Vec<ColorTarget>,
    edges: Vec<ColorTarget>,
    blends: Vec<ColorTarget>,
    /// LUT 本体（描述符直接引用 GpuImage 自带 view；析构随 GpuImage Drop）。
    area_lut: GpuImage,
    search_lut: GpuImage,
    infos: Vec<GpuBuffer>,
    sampler: vk::Sampler,
    extent: vk::Extent2D,
}

fn decode_lut_png(bytes: &[u8], role: &str) -> Result<(Vec<u8>, u32, u32)> {
    let mut image = Image::from_buffer(
        bytes,
        ImageType::Extension("png"),
        CompressedImageFormats::NONE,
        false,
        bevy::image::ImageSampler::default(),
        bevy::asset::RenderAssetUsages::all(),
    )
    .map_err(|e| VulkanError::Init(format!("{role} LUT PNG 解码失败: {e}")))?;
    let (w, h) = (image.width(), image.height());
    let data = image
        .data
        .take()
        .ok_or_else(|| VulkanError::Init(format!("{role} LUT PNG 数据缺席（AssetUsages 全用 + 未压即有数据）")))?;
    Ok((data, w, h))
}

/// LUT 解码 + 建图 + 直传（`role` 只用于报错文案）。
fn upload_lut(
    ctx: &Context,
    uploader: &mut crate::render::Uploader,
    bytes: &[u8],
    role: &str,
) -> Result<GpuImage> {
    let sharing = ctx.sharing_families();
    let (data, w, h) = decode_lut_png(bytes, role)?;
    let spec = crate::render::resident::images::ImageSpec {
        width: w,
        height: h,
        format: vk::Format::R8G8B8A8_UNORM,
        mip_levels: 1,
        sampler: bevy::image::ImageSamplerDescriptor::linear(),
    };
    let gpu = GpuImage::create(
        &ctx.device,
        &ctx.instance,
        ctx.physical_device,
        ctx.memory_contract(),
        &spec,
        &sharing,
    )?;
    let ticket = uploader.submit_batch(crate::render::core::device::transfer::UploadBatch {
        staging: data,
        image_uploads: vec![crate::render::core::device::transfer::StagingImageCopy {
            image: gpu.image(),
            src_offset: 0,
            width: w,
            height: h,
            mip_levels: 1,
        }],
        ..Default::default()
    })?;
    // 坑：必须当场等完。gpu 的 Drop 不等 GPU，后续任一步失败丢掉它时拷贝可能还在写
    if let Some(ticket) = ticket {
        uploader.wait_until(ticket)?;
    }
    Ok(gpu)
}

impl SmaaTargets {
    /// 建资源链：LUT 直传 → 布局 + 采样器 + pool（尺寸无关，只建一次）→
    /// 三目标 → UBO → 描述符集。
    ///
    /// # Errors
    /// 附件/LUT/描述符创建失败。
    pub fn new(ctx: &Context, uploader: &mut crate::render::Uploader, extent: vk::Extent2D) -> Result<Self> {
        // LUT 先于 this 建：半途失败时已建 GpuImage 随自身 Drop 回收
        let area_lut = upload_lut(ctx, uploader, SMAA_AREA_LUT_PNG, "area")?;
        let search_lut = upload_lut(ctx, uploader, SMAA_SEARCH_LUT_PNG, "search")?;
        let mut this = Self {
            device: ctx.device.clone(),
            set_layout_a: vk::DescriptorSetLayout::null(),
            set_layout_b: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets_edge: Vec::new(),
            sets_blend: Vec::new(),
            sets_nb: Vec::new(),
            ldrs: Vec::new(),
            edges: Vec::new(),
            blends: Vec::new(),
            area_lut,
            search_lut,
            infos: Vec::new(),
            sampler: vk::Sampler::null(),
            extent,
        };
        // 失败路径：已建对象都已落字段，由 Drop 回收
        unsafe { this.build_static() }?;
        this.build_targets(ctx, extent)?;
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            this.infos.push(this.mk_buffer(ctx, BufferRole::FrameUniform, u64::from(SMAA_INFO_SIZE))?);
        }
        unsafe { this.build_descriptors() }?;
        info!(
            "SMAA 资源就绪: LDR/edges/blend 三目标 + area 160×560 / search 64×16 LUT 直传 \
             × {} 帧槽 SmaaInfo",
            MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 尺寸相关目标：LDR/edges/blend ×帧槽（旧目标先清）。
    fn build_targets(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        self.ldrs.clear();
        self.edges.clear();
        self.blends.clear();
        self.extent = extent;
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            self.ldrs.push(ColorTarget::new(ctx, extent, vk::Format::R8G8B8A8_UNORM, false, "smaa ldr")?);
            self.edges.push(ColorTarget::new(ctx, extent, vk::Format::R8G8B8A8_UNORM, false, "smaa edges")?);
            self.blends.push(ColorTarget::new(ctx, extent, vk::Format::R8G8B8A8_UNORM, false, "smaa blend")?);
        }
        Ok(())
    }

    fn mk_buffer(&self, ctx: &Context, role: BufferRole, size: u64) -> Result<GpuBuffer> {
        let sharing = ctx.sharing_families();
        GpuBuffer::create_with_families(&self.device, ctx.memory_contract(), size, role, &sharing)
    }

    /// 尺寸无关对象：两套布局 + 采样器 + pool（容量只随帧槽定）。
    ///
    /// # Safety
    /// 只在 new 调一次；每个对象建成即落字段，失败路径由 Drop 回收。
    unsafe fn build_static(&mut self) -> Result<()> {
        unsafe {
        let device = &self.device;
        self.set_layout_a = SMAA_A_SET_DESC.create(device)?;
        self.set_layout_b = SMAA_B_SET_DESC.create(device)?;
        let slots = MAX_FRAMES_IN_FLIGHT;
        self.sampler = crate::render::resident::images::sampler_key(
            &bevy::image::ImageSamplerDescriptor::linear(),
        )
        .create_sampler(device)?;
        // 池容量从布局声明推导：每槽 edge/blend 两张 L_a + nb 一张 L_b
        let pool_sizes =
            merged_pool_sizes(&[(&SMAA_A_SET_DESC, 2 * slots as u32), (&SMAA_B_SET_DESC, slots as u32)]);
        self.pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets((3 * slots) as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        Ok(())
        }
    }

    /// 从 pool 分配三组描述符集并逐槽写入（pool 须为空：新建或刚 reset）。
    ///
    /// # Safety
    /// 静态对象、目标与 UBO 已建成功；失败路径由调用方回收。
    unsafe fn build_descriptors(&mut self) -> Result<()> {
        unsafe {
        let device = &self.device;
        let slots = MAX_FRAMES_IN_FLIGHT;
        self.sets_edge = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(self.pool)
                .set_layouts(&std::iter::repeat_n(self.set_layout_a, slots).collect::<Vec<_>>()),
        )?;
        self.sets_blend = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(self.pool)
                .set_layouts(&std::iter::repeat_n(self.set_layout_a, slots).collect::<Vec<_>>()),
        )?;
        self.sets_nb = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(self.pool)
                .set_layouts(&std::iter::repeat_n(self.set_layout_b, slots).collect::<Vec<_>>()),
        )?;
        // 三组集逐槽写，写入过声明校验（种类按 SMAA_A_SET_DESC / SMAA_B_SET_DESC 核对）
        let ro = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        let info_buf = |slot: usize| self.infos[slot % MAX_FRAMES_IN_FLIGHT].buffer();
        for slot in 0..slots {
            // edge set：b0 ldr + b1 参数 + b2 采样器
            let mut edge = DescriptorWriter::new(&SMAA_A_SET_DESC, self.sets_edge[slot]);
            edge.image(SMAA_BINDING_TEX0, self.ldrs[slot].view(), ro)
                .sampler(SMAA_BINDING_SAMPLER, self.sampler)
                .buffer(SMAA_BINDING_INFO, info_buf(slot), 0, vk::WHOLE_SIZE);
            edge.submit(device)?;
            // blend set：b0 edges + b3 search + b4 area
            let mut blend = DescriptorWriter::new(&SMAA_A_SET_DESC, self.sets_blend[slot]);
            blend
                .image(SMAA_BINDING_TEX0, self.edges[slot].view(), ro)
                .sampler(SMAA_BINDING_SAMPLER, self.sampler)
                .image(SMAA_BINDING_TEX1, self.search_lut.view(), ro)
                .image(SMAA_BINDING_TEX2, self.area_lut.view(), ro)
                .buffer(SMAA_BINDING_INFO, info_buf(slot), 0, vk::WHOLE_SIZE);
            blend.submit(device)?;
            // nb set：b0 ldr + b3 blend + b4 blend 采样器
            let mut nb = DescriptorWriter::new(&SMAA_B_SET_DESC, self.sets_nb[slot]);
            nb.image(SMAA_BINDING_TEX0, self.ldrs[slot].view(), ro)
                .sampler(SMAA_BINDING_SAMPLER, self.sampler)
                .image(SMAA_BINDING_TEX1, self.blends[slot].view(), ro)
                .sampler(SMAA_BINDING_TEX2, self.sampler)
                .buffer(SMAA_BINDING_INFO, info_buf(slot), 0, vk::WHOLE_SIZE);
            nb.submit(device)?;
        }
        Ok(())
        }
    }

    /// 随 swapchain 重建（闸门③，幂等）：三目标销毁重建 + pool reset 后重分配
    /// 描述符集。layout/采样器/pool/LUT 本体不动（与尺寸无关）。
    ///
    /// # Errors
    /// 附件/描述符重建失败。
    pub fn rebuild(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if self.ldr_extent() == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        unsafe {
            self.device
                .reset_descriptor_pool(self.pool, vk::DescriptorPoolResetFlags::empty())
        }?;
        self.sets_edge.clear();
        self.sets_blend.clear();
        self.sets_nb.clear();
        self.build_targets(ctx, extent)?;
        unsafe { self.build_descriptors() }?;
        info!("SMAA 三目标随 swapchain 重建: {extent:?}");
        Ok(())
    }

    /// LDR 目标尺寸（幂等判据）。
    pub fn ldr_extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// edge/blend 集布局。
    #[must_use]
    pub fn layout_a(&self) -> SetLayout {
        SetLayout::new(&SMAA_A_SET_DESC, self.set_layout_a)
    }

    /// neighborhood 集布局。
    #[must_use]
    pub fn layout_b(&self) -> SetLayout {
        SetLayout::new(&SMAA_B_SET_DESC, self.set_layout_b)
    }

    #[must_use]
    pub fn set_edge(&self, slot: usize) -> DescSet {
        DescSet::new(&SMAA_A_SET_DESC, self.sets_edge[slot])
    }
    #[must_use]
    pub fn set_blend(&self, slot: usize) -> DescSet {
        DescSet::new(&SMAA_A_SET_DESC, self.sets_blend[slot])
    }
    #[must_use]
    pub fn set_nb(&self, slot: usize) -> DescSet {
        DescSet::new(&SMAA_B_SET_DESC, self.sets_nb[slot])
    }
    #[must_use]
    pub fn info(&self, slot: usize) -> &GpuBuffer {
        &self.infos[slot]
    }

    /// SmaaInfo UBO（宿主写入口，fence 纪律内）。
    pub fn info_mut(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.infos[slot]
    }
    #[must_use]
    pub fn edges_view(&self, slot: usize) -> vk::ImageView {
        self.edges[slot].view()
    }
    #[must_use]
    pub fn edges_image(&self, slot: usize) -> vk::Image {
        self.edges[slot].image()
    }
    #[must_use]
    pub fn blend_view(&self, slot: usize) -> vk::ImageView {
        self.blends[slot].view()
    }
    #[must_use]
    pub fn blend_image(&self, slot: usize) -> vk::Image {
        self.blends[slot].image()
    }
    #[must_use]
    pub fn ldr_view(&self, slot: usize) -> vk::ImageView {
        self.ldrs[slot].view()
    }
    #[must_use]
    pub fn ldr_image(&self, slot: usize) -> vk::Image {
        self.ldrs[slot].image()
    }
}

impl Drop for SmaaTargets {
    fn drop(&mut self) {
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout_a != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout_a, None);
            }
            if self.set_layout_b != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout_b, None);
            }
            if self.sampler != vk::Sampler::null() {
                self.device.destroy_sampler(self.sampler, None);
            }
        }
    }
}


/// SMAA 三全屏管线（edge/blend/neighborhood；复用全屏管线骨架，
/// 本体持有——句柄拷贝陷阱照 8.4 教训）。
#[derive(Resource)]
pub struct SmaaPipelines {
    edge: FullscreenPipeline,
    blend: FullscreenPipeline,
    neighborhood: FullscreenPipeline,
}

impl SmaaPipelines {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        layouts: &SetLayouts,
    ) -> Result<Self> {
        Ok(Self {
            edge: FullscreenPipeline::new(ctx, &SMAA_EDGE_PIPELINE, layouts)?,
            blend: FullscreenPipeline::new(ctx, &SMAA_BLEND_PIPELINE, layouts)?,
            neighborhood: FullscreenPipeline::new(ctx, &SMAA_NEIGHBORHOOD_PIPELINE, layouts)?,
        })
    }

    #[must_use]
    pub(crate) fn edge_rec(&self) -> PipelineRef {
        self.edge.rec()
    }

    pub fn edge_pipeline(&self) -> vk::Pipeline {
        self.edge.pipeline()
    }
    #[must_use]
    pub fn edge_layout(&self) -> vk::PipelineLayout {
        self.edge.layout()
    }
    #[must_use]
    pub(crate) fn blend_rec(&self) -> PipelineRef {
        self.blend.rec()
    }

    pub fn blend_pipeline(&self) -> vk::Pipeline {
        self.blend.pipeline()
    }
    #[must_use]
    pub fn blend_layout(&self) -> vk::PipelineLayout {
        self.blend.layout()
    }
    #[must_use]
    pub(crate) fn neighborhood_rec(&self) -> PipelineRef {
        self.neighborhood.rec()
    }

    pub fn neighborhood_pipeline(&self) -> vk::Pipeline {
        self.neighborhood.pipeline()
    }
    #[must_use]
    pub fn neighborhood_layout(&self) -> vk::PipelineLayout {
        self.neighborhood.layout()
    }
}


/// 一帧 edge/blend 两 pass 的录制输入（smaa 开 = Some）。
#[derive(Clone, Copy)]
pub struct SmaaEdgeBlendDraw {
    pub edge: PipelineRef,
    pub blend: PipelineRef,
    pub edge_set: DescSet,
    pub blend_set: DescSet,
    pub edges_view: vk::ImageView,
    pub edges_image: vk::Image,
    pub blend_view: vk::ImageView,
    pub blend_image: vk::Image,
    pub extent: vk::Extent2D,
}

/// neighborhood 终点 pass 的录制输入（恒 Some）。
#[derive(Clone, Copy)]
pub struct SmaaNbDraw {
    pub nb: PipelineRef,
    pub nb_set: DescSet,
    pub ldr_view: vk::ImageView,
    pub ldr_image: vk::Image,
    pub ldr_extent: vk::Extent2D,
}
