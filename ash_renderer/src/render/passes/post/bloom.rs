//! Bloom 资源族（HDR 域 dual-Kawase 链）：半分辨率降采样链（prefilter
//! 软膝阈值 → N-1 级 down）+ 升采样链（`up[i]` = tent(`up[i+1]`) + `down[i]`）+
//! 参数 UBO（strength/enabled/tonemap）。
//!
//! 链路：composite HDR 线性输出 → prefilter（亮度阈值 + soft knee，读
//! G-buffer 采样集 b2/b4）→ down 链（逐级半分辨率）→ up 链（tent 上采样
//! 叠回降采样对应级）→ `up[0]` = 半分辨率 bloom 纹理，由 present tonemap
//! 采样叠加。三条管线都用全屏管线骨架（无深度、单 color、动态 viewport）。
//!
//! 布局：L_tex = [texture, sampler]（down 链源集）；L_up = [texture, texture,
//! sampler, uniform]（up 链双源 + 参数；tonemap 的 bloom 输入集复用同布局，b1 闲置）。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::put_f32;
use crate::common::error::Result;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::{Context, HDR_FORMAT, MAX_FRAMES_IN_FLIGHT};

use crate::render::core::pipeline::fullscreen::FullscreenPipeline;
use crate::render::core::target::color::ColorTarget;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::record::graphics::PipelineRef;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{merged_pool_sizes, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;

/// 降/升采样级数（半分辨率逐级）。
pub const BLOOM_LEVELS: usize = 5;

const BLOOM_BINDING_TEX0: u32 = 0;
const BLOOM_BINDING_TEX1: u32 = 1;
const BLOOM_BINDING_SAMPLER: u32 = 2;
const BLOOM_BINDING_PARAMS: u32 = 3;

const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// 降采样集：b0 源纹理 + b2 采样器。
pub(crate) static BLOOM_TEX_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "bloom_tex",
    &[
        BindingDesc::new(BLOOM_BINDING_TEX0, DescKind::SampledImage, FS),
        BindingDesc::new(BLOOM_BINDING_SAMPLER, DescKind::Sampler, FS),
    ],
);

/// 升采样集（也是 tonemap 的 bloom 输入集）：b0/b1 两张纹理 + b2 采样器 + b3 参数 UBO。
pub(crate) static BLOOM_UP_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "bloom_up",
    &[
        BindingDesc::new(BLOOM_BINDING_TEX0, DescKind::SampledImage, FS),
        BindingDesc::new(BLOOM_BINDING_TEX1, DescKind::SampledImage, FS),
        BindingDesc::new(BLOOM_BINDING_SAMPLER, DescKind::Sampler, FS),
        BindingDesc::new(BLOOM_BINDING_PARAMS, DescKind::UniformBuffer, FS),
    ],
);

pub(crate) static BLOOM_PREFILTER_PIPELINE: PipelineDesc = PipelineDesc {
    label: "bloom_prefilter",
    shader: &shader_abi::BLOOM_PREFILTER,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&GBUFFER_SAMPLED_SET_DESC],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

pub(crate) static BLOOM_DOWN_PIPELINE: PipelineDesc = PipelineDesc {
    label: "bloom_down",
    shader: &shader_abi::BLOOM_DOWN,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BLOOM_TEX_SET_DESC],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

pub(crate) static BLOOM_UP_PIPELINE: PipelineDesc = PipelineDesc {
    label: "bloom_up",
    shader: &shader_abi::BLOOM_UP,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BLOOM_UP_SET_DESC],
    push: None,
    color_formats: &[crate::render::HDR_FORMAT],
    depth_format: None,
};

/// 参数 UBO 字节长（5×f32 + 垫，std140 对齐 16）。
pub const BLOOM_PARAMS_SIZE: u32 = 32;

/// 参数 UBO 宿主侧取数（pack_bloom_params 排布）。
pub struct BloomParamsData {
    /// bloom 线性叠加系数（tonemap 前叠加）。
    pub strength: f32,
    /// bloom 开关（0/1）。
    pub enabled: f32,
    /// tonemap 开关（0/1，供 BRP A/B 对照；调试显示模式另由 render mode 旁路）。
    pub tonemap: f32,
    /// 曲线（0=Narkowicz / 1=ACES，见 [`TonemapCurve`]）。
    pub curve: f32,
    /// 亮度增益：tonemap 前 AP1 颜色乘它（1.0 = 纯 ACES，1.667 = ACES_LEGACY 观感）。
    pub gain: f32,
}

#[repr(C)]
struct BloomParamsLayout {
    strength: f32,
    enabled: f32,
    tonemap: f32,
    curve: f32,
    gain: f32,
    _pad: [f32; 3],
}

const _: () = {
    assert!(std::mem::size_of::<BloomParamsLayout>() as u32 == BLOOM_PARAMS_SIZE);
};

/// 参数打包（偏移按 BloomParamsLayout 排布，标量小端走 [`crate::common::bytes`]）。
#[must_use]
pub fn pack_bloom_params(d: &BloomParamsData) -> [u8; BLOOM_PARAMS_SIZE as usize] {
    let mut out = [0u8; BLOOM_PARAMS_SIZE as usize];
    put_f32(&mut out, 0, d.strength);
    put_f32(&mut out, 4, d.enabled);
    put_f32(&mut out, 8, d.tonemap);
    put_f32(&mut out, 12, d.curve);
    put_f32(&mut out, 16, d.gain);
    out
}

/// 色调映射曲线：Narkowicz 近似拟合（对照用）或完整 ACES（默认）。
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum TonemapCurve {
    Narkowicz,
    #[default]
    FilamentAces,
}

impl TonemapCurve {
    /// UBO 标量（BloomParamsLayout.curve 偏移）。
    #[must_use]
    pub fn as_f32(self) -> f32 {
        match self {
            Self::Narkowicz => 0.0,
            Self::FilamentAces => 1.0,
        }
    }
}

/// Bloom 开关与参数（BRP `bloom_set` 写，draw_frame 读）。
#[derive(bevy::prelude::Resource)]
pub struct BloomSettings {
    pub enabled: bool,
    pub strength: f32,
    pub tonemap: bool,
    pub curve: TonemapCurve,
    pub gain: f32,
}

impl Default for BloomSettings {
    fn default() -> Self {
        // gain 1.667 对应 ACES_LEGACY 观感；1.0 是纯 ACES，实测画面偏暗（均值亮度约 17%）
        Self { enabled: true, strength: 0.04, tonemap: true, curve: TonemapCurve::default(), gain: 1.667 }
    }
}

/// Bloom 链资源族：`down[N]`（prefilter+降采样目标）+ `up[N-1]`（升采样目标，
/// `up[0]` = 终品）+ 参数 UBO 帧槽环 + 两套描述符布局。
#[derive(bevy::prelude::Resource)]
pub struct BloomTargets {
    device: Device,
    set_layout_tex: vk::DescriptorSetLayout,
    set_layout_up: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// `down[i]` 源集（i=1..N-1，读 `down[i-1]`）。
    sets_down: Vec<vk::DescriptorSet>,
    /// `up[i]` 源集（i=0..N-2，读 `up[i+1]` 或 `down[N-1]` + `down[i]` + 参数）。
    sets_up: Vec<vk::DescriptorSet>,
    /// tonemap 第二集（每帧槽：up0 + 采样器 + 参数 UBO）。
    sets_params: Vec<vk::DescriptorSet>,
    down: Vec<ColorTarget>,
    up: Vec<ColorTarget>,
    ubos: Vec<GpuBuffer>,
    /// 各级尺寸（level 0 = 半分辨率）。
    extents: Vec<vk::Extent2D>,
    /// Bloom 链自有采样器（Drop 销毁）。
    sampler: vk::Sampler,
}

impl BloomTargets {
    /// 建资源链：布局 + 采样器 + pool（尺寸无关，只建一次）→ mip 目标 → UBO →
    /// 描述符集。
    ///
    /// # Errors
    /// 附件/描述符创建失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            set_layout_tex: vk::DescriptorSetLayout::null(),
            set_layout_up: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets_down: Vec::new(),
            sets_up: Vec::new(),
            sets_params: Vec::new(),
            down: Vec::new(),
            up: Vec::new(),
            ubos: Vec::new(),
            extents: Vec::new(),
            sampler: vk::Sampler::null(),
        };
        // 失败路径：已建对象都已落字段，由 Drop 回收
        unsafe { this.build_static() }?;
        this.build_targets(ctx, extent)?;
        let sharing = ctx.sharing_families();
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            this.ubos.push(GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                u64::from(BLOOM_PARAMS_SIZE),
                BufferRole::FrameUniform,
                &sharing,
            )?);
        }
        unsafe { this.build_descriptors() }?;
        info!(
            "Bloom 资源就绪: {BLOOM_LEVELS} 级 dual-Kawase（半分辨率起）× {} 帧槽参数 UBO，\
             阈值 1.0 + soft knee（线性叠加系数随 BRP bloom_set）",
            MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 随 swapchain 重建（尺寸没变直接返回）：重建链目标，reset pool 后重分配描述符集
    /// （自带 device_wait_idle，在飞帧不会再引用旧目标）。layout/采样器/pool 本体与尺寸
    /// 无关，不动。
    ///
    /// # Errors
    /// 附件/描述符重建失败。
    pub fn rebuild(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if self.extents.first() == Some(&vk::Extent2D {
            width: extent.width / 2,
            height: extent.height / 2,
        }) {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        unsafe {
            self.device
                .reset_descriptor_pool(self.pool, vk::DescriptorPoolResetFlags::empty())
        }?;
        self.sets_down.clear();
        self.sets_up.clear();
        self.sets_params.clear();
        self.build_targets(ctx, extent)?;
        unsafe { self.build_descriptors() }?;
        info!(
            "Bloom 链随 swapchain 重建: 级 0 = {}×{}",
            self.extents[0].width, self.extents[0].height,
        );
        Ok(())
    }

    /// 尺寸相关目标：`down[N]` + `up[N-1]` + 各级 extent（旧目标先清）。
    fn build_targets(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        self.down.clear();
        self.up.clear();
        self.extents.clear();
        let mut w = extent.width;
        let mut h = extent.height;
        for k in 0..BLOOM_LEVELS {
            w = (w / 2).max(1);
            h = (h / 2).max(1);
            let level = vk::Extent2D { width: w, height: h };
            self.extents.push(level);
            self.down.push(ColorTarget::new(ctx, level, HDR_FORMAT, false, "bloom down")?);
            if k < BLOOM_LEVELS - 1 {
                // up 链少一级（最小级 down[N-1] 直接作 up 源）；up[i] 与
                // extents[i] 同尺寸（建在 extents[i+1] 上会 renderArea 越界 VUID）
                self.up.push(ColorTarget::new(ctx, level, HDR_FORMAT, false, "bloom up")?);
            }
        }
        Ok(())
    }

    /// 尺寸无关对象：两套布局 + 自有采样器 + pool（容量只随级数/帧槽定）。
    ///
    /// # Safety
    /// 只在 new 调一次；每个对象建成即落字段，失败路径由 Drop 回收。
    unsafe fn build_static(&mut self) -> Result<()> {
        unsafe {
        let device = &self.device;
        self.set_layout_tex = BLOOM_TEX_SET_DESC.create(device)?;
        self.set_layout_up = BLOOM_UP_SET_DESC.create(device)?;
        // 自有线性采样器：随本资源 Drop 销毁，不占 bindless 表的采样器槽
        self.sampler = crate::render::resident::images::sampler_key(
            &bevy::image::ImageSamplerDescriptor::linear(),
        )
        .create_sampler(device)?;
        // 池容量从布局声明推导（分配即按全 binding 扣，与是否写入无关）：
        // down 集 (N-1)×slots 张 L_tex，up 集 (N-1)×slots + tonemap 集 slots 张 L_up
        let n = BLOOM_LEVELS as u32;
        let slots = MAX_FRAMES_IN_FLIGHT as u32;
        let sets_total = 2 * (n - 1) * slots + slots;
        let pool_sizes =
            merged_pool_sizes(&[(&BLOOM_TEX_SET_DESC, (n - 1) * slots), (&BLOOM_UP_SET_DESC, (n - 1) * slots + slots)]);
        self.pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(sets_total)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        Ok(())
        }
    }

    /// 从 pool 分配三组描述符集并写入（pool 须为空：新建或刚 reset）。
    ///
    /// # Safety
    /// 静态对象与目标已建成功；失败路径由调用方回收。
    unsafe fn build_descriptors(&mut self) -> Result<()> {
        unsafe {
        let device = &self.device;
        let sampler = self.sampler;
        let n = self.down.len();
        let slots = MAX_FRAMES_IN_FLIGHT;
        let alloc = |layout: vk::DescriptorSetLayout, count: usize| -> Result<Vec<vk::DescriptorSet>> {
            Ok(device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&std::iter::repeat_n(layout, count).collect::<Vec<_>>()),
            )?)
        };
        self.sets_down = alloc(self.set_layout_tex, (n - 1) * slots)?;
        self.sets_up = alloc(self.set_layout_up, (n - 1) * slots)?;
        self.sets_params = alloc(self.set_layout_up, slots)?;
        // 写入按布局声明核对种类，写满才提交
        let ro = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        for i in 1..n {
            for slot in 0..slots {
                let set_i = (i - 1) * slots + slot;
                let mut writer = DescriptorWriter::new(&BLOOM_TEX_SET_DESC, self.sets_down[set_i]);
                writer.image(BLOOM_BINDING_TEX0, self.down[i - 1].view(), ro);
                writer.sampler(BLOOM_BINDING_SAMPLER, sampler);
                writer.submit(device)?;
            }
        }
        for i in 0..n - 1 {
            for slot in 0..slots {
                let set_i = i * slots + slot;
                let up_src = if i + 1 == n - 1 { self.down[n - 1].view() } else { self.up[i + 1].view() };
                let mut writer = DescriptorWriter::new(&BLOOM_UP_SET_DESC, self.sets_up[set_i]);
                writer
                    .image(BLOOM_BINDING_TEX0, up_src, ro)
                    .image(BLOOM_BINDING_TEX1, self.down[i].view(), ro)
                    .sampler(BLOOM_BINDING_SAMPLER, sampler)
                    .buffer(BLOOM_BINDING_PARAMS, self.ubos[slot].buffer(), 0, vk::WHOLE_SIZE);
                writer.submit(device)?;
            }
        }
        for slot in 0..slots {
            let mut writer = DescriptorWriter::new(&BLOOM_UP_SET_DESC, self.sets_params[slot]);
            writer
                .image(BLOOM_BINDING_TEX0, self.up[0].view(), ro)
                .image(BLOOM_BINDING_TEX1, self.up[0].view(), ro)
                .sampler(BLOOM_BINDING_SAMPLER, sampler)
                .buffer(BLOOM_BINDING_PARAMS, self.ubos[slot].buffer(), 0, vk::WHOLE_SIZE);
            writer.submit(device)?;
        }
        Ok(())
        }
    }

    /// 升采样集布局（up 管线与 tonemap 的 bloom 输入集共用）。
    #[must_use]
    pub fn up_layout(&self) -> SetLayout {
        SetLayout::new(&BLOOM_UP_SET_DESC, self.set_layout_up)
    }

    /// 降采样集布局（down 管线用）。
    #[must_use]
    pub fn tex_layout(&self) -> SetLayout {
        SetLayout::new(&BLOOM_TEX_SET_DESC, self.set_layout_tex)
    }

    /// 本帧槽 tonemap 参数集（up0 + 采样器 + 参数 UBO）。
    #[must_use]
    pub fn params_set(&self, slot: usize) -> DescSet {
        DescSet::new(&BLOOM_UP_SET_DESC, self.sets_params[slot])
    }

    /// 本帧槽 down 链源集（`level` = 1..N-1，读 down[level-1]）。
    #[must_use]
    pub fn set_down(&self, level: usize, slot: usize) -> DescSet {
        DescSet::new(&BLOOM_TEX_SET_DESC, self.sets_down[(level - 1) * MAX_FRAMES_IN_FLIGHT + slot])
    }

    /// 本帧槽 up 链源集（`level` = 0..N-2）。
    #[must_use]
    pub fn set_up(&self, level: usize, slot: usize) -> DescSet {
        DescSet::new(&BLOOM_UP_SET_DESC, self.sets_up[level * MAX_FRAMES_IN_FLIGHT + slot])
    }

    /// 参数 UBO（宿主写入口，fence 纪律内）。
    pub fn ubo(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.ubos[slot]
    }

    /// 级 i 的渲染目标 extent。
    pub fn level_extent(&self, level: usize) -> vk::Extent2D {
        self.extents[level]
    }

    /// image/view 访问（录制屏障与附件用）。
    #[must_use]
    pub fn down_view(&self, level: usize) -> vk::ImageView {
        self.down[level].view()
    }
    #[must_use]
    pub fn down_image(&self, level: usize) -> vk::Image {
        self.down[level].image()
    }
    #[must_use]
    pub fn up_view(&self, level: usize) -> vk::ImageView {
        self.up[level].view()
    }
    #[must_use]
    pub fn up_image(&self, level: usize) -> vk::Image {
        self.up[level].image()
    }
}

impl Drop for BloomTargets {
    fn drop(&mut self) {
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout_tex != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout_tex, None);
            }
            if self.set_layout_up != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout_up, None);
            }
            if self.sampler != vk::Sampler::null() {
                self.device.destroy_sampler(self.sampler, None);
            }
        }
    }
}

/// Bloom 三条全屏管线（prefilter/down/up）。
/// 坑：必须持有 FullscreenPipeline 本体；只存句柄拷贝的话，临时值 Drop 时管线就被
/// 销毁（验证层报 Invalid VkPipeline）。
#[derive(bevy::prelude::Resource)]
pub struct BloomPipelines {
    prefilter: FullscreenPipeline,
    down: FullscreenPipeline,
    up: FullscreenPipeline,
}

impl BloomPipelines {
    /// # Errors
    /// 配方对账不过，或管线创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let prefilter = FullscreenPipeline::new(ctx, &BLOOM_PREFILTER_PIPELINE, layouts)?;
        let down = FullscreenPipeline::new(ctx, &BLOOM_DOWN_PIPELINE, layouts)?;
        let up = FullscreenPipeline::new(ctx, &BLOOM_UP_PIPELINE, layouts)?;
        Ok(Self { prefilter, down, up })
    }

    #[must_use]
    pub(crate) fn prefilter_rec(&self) -> PipelineRef {
        self.prefilter.rec()
    }

    #[must_use]
    pub(crate) fn down_rec(&self) -> PipelineRef {
        self.down.rec()
    }

    #[must_use]
    pub(crate) fn up_rec(&self) -> PipelineRef {
        self.up.rec()
    }
}

/// 一帧 Bloom + tonemap 录制输入（draw_frame 组装）。
#[derive(Clone, Copy)]
pub struct BloomFrameDraw {
    pub prefilter: PipelineRef,
    pub down: PipelineRef,
    pub up: PipelineRef,
    pub gbuffer_set: DescSet,
    pub down_sets: [DescSet; BLOOM_LEVELS - 1],
    pub up_sets: [DescSet; BLOOM_LEVELS - 1],
    pub images_down: [vk::Image; BLOOM_LEVELS],
    pub images_up: [vk::Image; BLOOM_LEVELS - 1],
    pub views_down: [vk::ImageView; BLOOM_LEVELS],
    pub views_up: [vk::ImageView; BLOOM_LEVELS - 1],
    /// 各级 extent（viewport 用）。
    pub extents: [vk::Extent2D; BLOOM_LEVELS],
}
