//! 8.4 Bloom 资源族（HDR 域 dual-Kawase 链）：半分辨率降采样链（prefilter
//! 软膝阈值 → N-1 级 down）+ 升采样链（up[i] = tent(up[i+1]) + down[i]）+
//! 参数 UBO（strength/enabled/tonemap）。
//!
//! 链路：composite HDR 线性输出 → prefilter（亮度阈值 + soft knee，读
//! G-buffer 采样集 b2/b4）→ down 链（逐级半分辨率）→ up 链（tent 上采样
//! 叠回降采样对应级）→ up[0] = 半分辨率 bloom 纹理，由 present tonemap
//! 采样叠加。全屏 pass 三件套复用 FullscreenPipeline 脚手架（无深度、单
//! color、动态 viewport）。
//!
//! 布局族：L_tex = [texture, sampler]（down 链源集）；L_up = [texture, texture,
//! sampler, uniform]（up 链双源 + 参数；tonemap 第二集复用同布局，b1 合法
//! 闲置）。binding 收在本资源族自有布局，G-buffer 契约零触碰。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::Result;
use crate::render::resources::{BufferRole, GpuBuffer};
use crate::render::{Context, HDR_FORMAT, MAX_FRAMES_IN_FLIGHT};

use super::fullscreen_pipeline::FullscreenPipeline;
use super::gbuffer::ColorTarget;

const BLOOM_PREFILTER_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/bloom_prefilter.spv"));
const BLOOM_DOWN_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bloom_down.spv"));
const BLOOM_UP_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bloom_up.spv"));

/// 降/升采样级数（半分辨率逐级）。
pub const BLOOM_LEVELS: usize = 5;

pub const BLOOM_BINDING_TEX0: u32 = 0;
pub const BLOOM_BINDING_TEX1: u32 = 1;
pub const BLOOM_BINDING_SAMPLER: u32 = 2;
pub const BLOOM_BINDING_PARAMS: u32 = 3;

/// 参数 UBO 字节长（3×f32 + 13 垫）。
pub const BLOOM_PARAMS_SIZE: u32 = 16;

/// 参数 UBO 宿主侧取数（pack_bloom_params 排布）。
pub struct BloomParamsData {
    /// bloom 线性叠加系数（tonemap 前叠加）。
    pub strength: f32,
    /// bloom 开关（0/1）。
    pub enabled: f32,
    /// tonemap 开关（0/1；debug 仪器模式由 render mode 旁路面承接，此旗标
    /// 供 BRP A/B 对照用）。
    pub tonemap: f32,
}

#[repr(C)]
#[allow(dead_code)]
struct BloomParamsLayout {
    strength: f32,
    enabled: f32,
    tonemap: f32,
    _pad: f32,
}

const _: () = {
    assert!(std::mem::size_of::<BloomParamsLayout>() as u32 == BLOOM_PARAMS_SIZE);
};

/// 参数打包（偏移按 BloomParamsLayout 手工排布）。
#[must_use]
pub fn pack_bloom_params(d: &BloomParamsData) -> [u8; BLOOM_PARAMS_SIZE as usize] {
    let mut out = [0u8; BLOOM_PARAMS_SIZE as usize];
    out[0..4].copy_from_slice(&d.strength.to_le_bytes());
    out[4..8].copy_from_slice(&d.enabled.to_le_bytes());
    out[8..12].copy_from_slice(&d.tonemap.to_le_bytes());
    out
}

/// Bloom 开关与参数（BRP `bloom_set` 写，draw_frame 读）。
#[derive(bevy::prelude::Resource)]
pub struct BloomSettings {
    pub enabled: bool,
    pub strength: f32,
    pub tonemap: bool,
}

impl Default for BloomSettings {
    fn default() -> Self {
        Self { enabled: true, strength: 0.04, tonemap: true }
    }
}

/// Bloom 链资源族：down[N]（prefilter+降采样目标）+ up[N-1]（升采样目标，
/// up[0] = 终品）+ 参数 UBO 帧槽环 + 两套描述符布局。
#[derive(bevy::prelude::Resource)]
pub struct BloomTargets {
    device: Device,
    set_layout_tex: vk::DescriptorSetLayout,
    set_layout_up: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// down[i] 源集（i=1..N-1，读 down[i-1]）。
    sets_down: Vec<vk::DescriptorSet>,
    /// up[i] 源集（i=0..N-2，读 up[i+1] 或 down[N-1] + down[i] + 参数）。
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
    /// 建资源链：mip 目标 → UBO → 布局 → pool + 描述符。
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
        let built = (|| -> Result<()> {
            let mut w = extent.width;
            let mut h = extent.height;
            for k in 0..BLOOM_LEVELS {
                w = (w / 2).max(1);
                h = (h / 2).max(1);
                this.extents.push(vk::Extent2D { width: w, height: h });
                this.down.push(ColorTarget::new(
                    ctx,
                    vk::Extent2D { width: w, height: h },
                    HDR_FORMAT,
                    false,
                    "bloom down",
                )?);
                if k < BLOOM_LEVELS - 1 {
                    // up 链少一级（最小级 down[N-1] 直接作 up 源）；up[i] 与
                    // extents[i] 同尺寸（首版建在 extents[i+1] 上——renderArea
                    // 越界 VUID 实抓即修）
                    this.up.push(ColorTarget::new(
                        ctx,
                        vk::Extent2D { width: w, height: h },
                        HDR_FORMAT,
                        false,
                        "bloom up",
                    )?);
                }
            }
            for _ in 0..MAX_FRAMES_IN_FLIGHT {
                this.ubos.push(this
                    .mk_buffer(ctx, BufferRole::FrameUniform, u64::from(BLOOM_PARAMS_SIZE))?);
            }
            unsafe { this.build_descriptors() }?;
            Ok(())
        })();
        built?;
        info!(
            "Bloom 资源就绪: {BLOOM_LEVELS} 级 dual-Kawase（半分辨率起）× {} 帧槽参数 UBO，\
             阈值 1.0 + soft knee（线性叠加系数随 BRP bloom_set）",
            MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 随 swapchain 重建（闸门③，幂等——extent 不变空手而归）：销毁链目标与
    /// 描述符重建（自带 device_wait_idle，在飞旧 bloom 目标不可能被引用）。
    /// layout 本体不动（与尺寸无关，描述符指向重写）。
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
        self.down.clear();
        self.up.clear();
        if self.pool != vk::DescriptorPool::null() {
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
            self.pool = vk::DescriptorPool::null();
        }
        self.sets_down.clear();
        self.sets_up.clear();
        self.sets_params.clear();
        let mut w = extent.width;
        let mut h = extent.height;
        for k in 0..BLOOM_LEVELS {
            w = (w / 2).max(1);
            h = (h / 2).max(1);
            self.extents[k] = vk::Extent2D { width: w, height: h };
            self.down.push(ColorTarget::new(
                ctx,
                vk::Extent2D { width: w, height: h },
                HDR_FORMAT,
                false,
                "bloom down",
            )?);
            if k < BLOOM_LEVELS - 1 {
                self.up.push(ColorTarget::new(
                    ctx,
                    vk::Extent2D { width: w, height: h },
                    HDR_FORMAT,
                    false,
                    "bloom up",
                )?);
            }
        }
        unsafe { self.build_descriptors() }?;
        info!("Bloom 链随 swapchain 重建: 级 0 = {}×{}", w, h);
        Ok(())
    }

    fn mk_buffer(&self, ctx: &Context, role: BufferRole, size: u64) -> Result<GpuBuffer> {
        let sharing: Vec<u32> = if ctx.transfer_queue_family_index != ctx.queue_family_index {
            vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
        } else {
            Vec::new()
        };
        GpuBuffer::create_with_families(&self.device, ctx.memory_contract(), size, role, &sharing)
    }

    /// # Safety
    /// 目标已建成功；失败路径由调用方回收。
    unsafe fn build_descriptors(&mut self) -> Result<()> {
        unsafe {
        let device = &self.device;
        let stages = vk::ShaderStageFlags::FRAGMENT;
        let tex = |binding: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(stages)
        };
        self.set_layout_tex = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                tex(BLOOM_BINDING_TEX0),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(BLOOM_BINDING_SAMPLER)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(stages),
            ]),
            None,
        )?;
        self.set_layout_up = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                tex(BLOOM_BINDING_TEX0),
                tex(BLOOM_BINDING_TEX1),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(BLOOM_BINDING_SAMPLER)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(stages),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(BLOOM_BINDING_PARAMS)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(stages),
            ]),
            None,
        )?;
        let n = self.down.len();
        let slots = MAX_FRAMES_IN_FLIGHT;
        let sets_total = (n - 1) * slots + (n - 1) * slots + slots;
        self.pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(sets_total as u32)
                .pool_sizes(&[
                    vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::SAMPLED_IMAGE)
                        .descriptor_count((3 * (n - 1) * slots + slots) as u32),
                    vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::SAMPLER)
                        .descriptor_count(((n - 1) * 2 * slots + slots) as u32),
                    vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::UNIFORM_BUFFER)
                        .descriptor_count(((n - 1) * slots + slots) as u32),
                ]),
            None,
        )?;
        // Bloom 链自有采样器（线性 + CLAMP_TO_EDGE；本体随本资源 Drop 销毁
        // ——不走描述符表去重账，账本采样器槽留给贴图面）
        let sampler = crate::render::assets::images::sampler_key(
            &bevy::image::ImageSamplerDescriptor::linear(),
        )
        .create_sampler(device)?;
        let alloc = |layout: vk::DescriptorSetLayout, count: usize| -> Result<Vec<vk::DescriptorSet>> {
            let sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&std::iter::repeat_n(layout, count).collect::<Vec<_>>()),
            )?;
            Ok(sets)
        };
        self.sets_down = alloc(self.set_layout_tex, (n - 1) * slots)?;
        self.sets_up = alloc(self.set_layout_up, (n - 1) * slots)?;
        self.sets_params = alloc(self.set_layout_up, slots)?;
        let mut infos: Vec<vk::DescriptorImageInfo> = Vec::new();
        let mut buf_infos: Vec<vk::DescriptorBufferInfo> = Vec::new();
        let mut plan: Vec<(usize, u32, vk::DescriptorType, usize, bool)> = Vec::new();
        let push_img = |infos: &mut Vec<vk::DescriptorImageInfo>,
                        view: vk::ImageView,
                        plan: &mut Vec<(usize, u32, vk::DescriptorType, usize, bool)>,
                        set_i: usize,
                        binding: u32| {
            infos.push(
                vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            );
            plan.push((set_i, binding, vk::DescriptorType::SAMPLED_IMAGE, infos.len() - 1, false));
        };
        for i in 1..n {
            for slot in 0..slots {
                let set_i = (i - 1) * slots + slot;
                push_img(&mut infos, self.down[i - 1].view(), &mut plan, set_i, BLOOM_BINDING_TEX0);
                infos.push(
                    vk::DescriptorImageInfo::default().sampler(sampler),
                );
                plan.push((set_i, BLOOM_BINDING_SAMPLER, vk::DescriptorType::SAMPLER,
                    infos.len() - 1, false));
            }
        }
        for i in 0..n - 1 {
            for slot in 0..slots {
                let set_i = (n - 1) * slots + i * slots + slot;
                let up_src = if i + 1 == n - 1 { self.down[n - 1].view() } else { self.up[i + 1].view() };
                push_img(&mut infos, up_src, &mut plan, set_i, BLOOM_BINDING_TEX0);
                push_img(&mut infos, self.down[i].view(), &mut plan, set_i, BLOOM_BINDING_TEX1);
                infos.push(vk::DescriptorImageInfo::default().sampler(sampler));
                plan.push((set_i, BLOOM_BINDING_SAMPLER, vk::DescriptorType::SAMPLER,
                    infos.len() - 1, false));
                buf_infos.push(
                    vk::DescriptorBufferInfo::default()
                        .buffer(self.ubos[slot].buffer())
                        .offset(0)
                        .range(vk::WHOLE_SIZE),
                );
                plan.push((set_i, BLOOM_BINDING_PARAMS, vk::DescriptorType::UNIFORM_BUFFER,
                    buf_infos.len() - 1, true));
            }
        }
        for slot in 0..slots {
            let set_i = (n - 1) * 2 * slots + slot;
            push_img(&mut infos, self.up[0].view(), &mut plan, set_i, BLOOM_BINDING_TEX0);
            push_img(&mut infos, self.up[0].view(), &mut plan, set_i, BLOOM_BINDING_TEX1);
            infos.push(vk::DescriptorImageInfo::default().sampler(sampler));
            plan.push((set_i, BLOOM_BINDING_SAMPLER, vk::DescriptorType::SAMPLER,
                infos.len() - 1, false));
            buf_infos.push(
                vk::DescriptorBufferInfo::default()
                    .buffer(self.ubos[slot].buffer())
                    .offset(0)
                    .range(vk::WHOLE_SIZE),
            );
            plan.push((set_i, BLOOM_BINDING_PARAMS, vk::DescriptorType::UNIFORM_BUFFER,
                buf_infos.len() - 1, true));
        }
        let writes: Vec<vk::WriteDescriptorSet> = plan
            .iter()
            .map(|&(set_i, binding, ty, idx, is_buf)| {
                if is_buf {
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.all_sets()[set_i])
                        .dst_binding(binding)
                        .descriptor_type(ty)
                        .buffer_info(&buf_infos[idx..idx + 1])
                } else {
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.all_sets()[set_i])
                        .dst_binding(binding)
                        .descriptor_type(ty)
                        .image_info(&infos[idx..idx + 1])
                }
            })
            .collect();
        device.update_descriptor_sets(&writes, &[]);
        Ok(())
        }
    }

    fn all_sets(&self) -> Vec<vk::DescriptorSet> {
        self.sets_down
            .iter()
            .chain(self.sets_up.iter())
            .chain(self.sets_params.iter())
            .copied()
            .collect()
    }

    /// set_layout_up 句柄（tonemap 第二集 layout 借用）。
    #[must_use]
    pub fn set_layout_up(&self) -> vk::DescriptorSetLayout {
        self.set_layout_up
    }

    /// set_layout_tex 句柄（down 管线 layout 借用）。
    #[must_use]
    pub fn set_layout_tex(&self) -> vk::DescriptorSetLayout {
        self.set_layout_tex
    }

    /// 本帧槽 tonemap 参数集（up0 + 采样器 + 参数 UBO）。
    #[must_use]
    pub fn params_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets_params[slot]
    }

    /// 本帧槽 down 链源集（i=1..N-1 → 下标 (i-1)*slots+slot）。
    #[must_use]
    pub fn set_down(&self, level: usize, slot: usize) -> vk::DescriptorSet {
        self.sets_down[(level - 1) * MAX_FRAMES_IN_FLIGHT + slot]
    }

    /// 本帧槽 up 链源集（i=0..N-2 → 下标 (N-1)*slots + i*slots + slot）。
    #[must_use]
    pub fn set_up(&self, level: usize, slot: usize) -> vk::DescriptorSet {
        self.sets_up[level * MAX_FRAMES_IN_FLIGHT + slot]
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

// # Safety:句柄本质是整数,创建/销毁只在持有方线程;方法只读句柄
unsafe impl Send for BloomTargets {}
unsafe impl Sync for BloomTargets {}

/// Bloom 三全屏管线（prefilter/down/up；复用 FullscreenPipeline 脚手架）。
/// （drop：FullscreenPipeline 自带 Drop，本结构只持句柄拷贝——layout/pipeline
/// 句柄经由 FullscreenPipeline 的值移动保留，析构随移动值 Drop 完成。）
#[derive(bevy::prelude::Resource)]
pub struct BloomPipelines {
    /// 持有 FullscreenPipeline 本体（句柄拷贝会随临时值 Drop 把管线销毁
    /// ——首跑实抓 Invalid VkPipeline，即修）。
    prefilter: FullscreenPipeline,
    down: FullscreenPipeline,
    up: FullscreenPipeline,
}

impl BloomPipelines {
    /// prefilter 管线句柄。
    #[must_use]
    pub fn prefilter_pipeline(&self) -> vk::Pipeline {
        self.prefilter.pipeline()
    }
    /// prefilter layout 句柄。
    #[must_use]
    pub fn prefilter_layout(&self) -> vk::PipelineLayout {
        self.prefilter.layout()
    }
    /// down 管线句柄。
    #[must_use]
    pub fn down_pipeline(&self) -> vk::Pipeline {
        self.down.pipeline()
    }
    /// down layout 句柄。
    #[must_use]
    pub fn down_layout(&self) -> vk::PipelineLayout {
        self.down.layout()
    }
    /// up 管线句柄。
    #[must_use]
    pub fn up_pipeline(&self) -> vk::Pipeline {
        self.up.pipeline()
    }
    /// up layout 句柄。
    #[must_use]
    pub fn up_layout(&self) -> vk::PipelineLayout {
        self.up.layout()
    }
}

impl BloomPipelines {
    /// 建三全屏管线（shader module/layout/pipeline 由 FullscreenPipeline 承包）。
    ///
    /// # Errors
    /// shader 编译/管线创建失败。
    pub fn new(
        ctx: &Context,
        gbuffer_layout: vk::DescriptorSetLayout,
        layout_tex: vk::DescriptorSetLayout,
        layout_up: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let pre = FullscreenPipeline::new(
            ctx,
            BLOOM_PREFILTER_SPV,
            HDR_FORMAT,
            &[gbuffer_layout],
        )?;
        let down = FullscreenPipeline::new(ctx, BLOOM_DOWN_SPV, HDR_FORMAT, &[layout_tex])?;
        let up = FullscreenPipeline::new(ctx, BLOOM_UP_SPV, HDR_FORMAT, &[layout_up])?;
        Ok(Self { prefilter: pre, down, up })
    }
}

/// 一帧 Bloom + tonemap 录制输入（draw_frame 组装）。
#[derive(Clone, Copy)]
pub struct BloomFrameDraw {
    pub prefilter_pipeline: vk::Pipeline,
    pub prefilter_layout: vk::PipelineLayout,
    pub down_pipeline: vk::Pipeline,
    pub down_layout: vk::PipelineLayout,
    pub up_pipeline: vk::Pipeline,
    pub up_layout: vk::PipelineLayout,
    pub gbuffer_set: vk::DescriptorSet,
    pub down_sets: [vk::DescriptorSet; BLOOM_LEVELS - 1],
    pub up_sets: [vk::DescriptorSet; BLOOM_LEVELS - 1],
    pub images_down: [vk::Image; BLOOM_LEVELS],
    pub images_up: [vk::Image; BLOOM_LEVELS - 1],
    pub views_down: [vk::ImageView; BLOOM_LEVELS],
    pub views_up: [vk::ImageView; BLOOM_LEVELS - 1],
    /// 各级 extent（viewport 用）。
    pub extents: [vk::Extent2D; BLOOM_LEVELS],
}
