//! 颜色校验面板的采样侧（9.9）：三域直方图 bins 与采样 compute 管线。
//!
//! 数据流：tonemap 出场屏障之后（LDR 已 `SHADER_READ_ONLY`）、SMAA edge 之前录一条
//! compute dispatch，按 [`COLOR_SCOPE_STRIDE`] 抽样三个位置——
//!
//! - A 域：present 采样到的原始 HDR（未加 Bloom、未乘曝光）；
//! - B 域：曲线输入色（`common/tonemap_shared.wgsl` 的 `tonemap_input_color`，
//!   与 present 同式）；
//! - C 域：tonemap 写进 LDR 的线性 UNORM 值（读回后做 sRGB 编码再进轴）。
//!
//! bins 布局见 `shaders/post/color_scope.wgsl` 头注；面板（egui「颜色」页）与 BRP
//! `color_scope_get` 读同一份 bins，CPU 侧不另立统计口径。
//!
//! 开关语义：`ColorScopeSettings::enabled` 默认 `false`，关时零 dispatch、零回读、
//! 零描述符写（录制侧整段跳过，命令序列与没有本模块时相同）。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{
    merged_pool_sizes, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts,
};
use crate::render::core::binding::shader_abi;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::device::readback::OneShotReadback;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::barrier::MemoryBarrierDesc;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::Context;

/// 采样步长：每 2 像素取 1（1600×900 约 36 万样本）。
pub const COLOR_SCOPE_STRIDE: u32 = 4;
/// 头区 u32 数。
pub const COLOR_SCOPE_HEADER_U32: u32 = 16;
/// 单个域的 u32 数：luma 256 + RGB 3×256 + 计数 5（样本 / 低于轴 / 高于轴 / ≤0 / ≥1）。
pub const COLOR_SCOPE_DOMAIN_U32: u32 = 1029;
/// C 域色度图边长。
pub const COLOR_SCOPE_CBCR_EDGE: u32 = 64;
/// C 域色度 u32 数。
pub const COLOR_SCOPE_CBCR_U32: u32 = COLOR_SCOPE_CBCR_EDGE * COLOR_SCOPE_CBCR_EDGE;
/// bins 总 u32 数（7 199，约 28.8 KB）。
pub const COLOR_SCOPE_BINS_U32: u32 =
    COLOR_SCOPE_HEADER_U32 + 3 * COLOR_SCOPE_DOMAIN_U32 + COLOR_SCOPE_CBCR_U32;
/// bins 字节长。
pub const COLOR_SCOPE_BINS_BYTES: u64 = COLOR_SCOPE_BINS_U32 as u64 * 4;
/// bins magic（`"CSCP"` 小端）：面板/BRP 据此判断还没采过样。
pub const COLOR_SCOPE_MAGIC: u32 = u32::from_le_bytes(*b"CSCP");

/// 采样 compute 的 push constant 字节数（一个 u32：采样帧号）。
pub const COLOR_SCOPE_PUSH_BYTES: u32 = 4;

/// A/B 直方图的 EV 下限（与 `shaders/post/color_scope.wgsl` 的 `AXIS_MIN_EV` 同源）。
pub const COLOR_SCOPE_AXIS_MIN_EV: f32 = -16.0;
/// A/B 直方图的 EV 跨度（与 shader 的 `AXIS_EV_SPAN` 同源）。
pub const COLOR_SCOPE_AXIS_SPAN_EV: f32 = 24.0;

/// 头里宽 / 高 / 帧号的下标（其余下标见 shader 头注）。
pub const COLOR_SCOPE_HEADER_WIDTH: usize = 2;
/// 头里高的下标。
pub const COLOR_SCOPE_HEADER_HEIGHT: usize = 3;
/// 头里帧号的下标：BRP 用"当前帧号 − 它"算 `frame_age`。
pub const COLOR_SCOPE_HEADER_FRAME: usize = 4;

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 采样 compute 的描述符集：三域纹理 + 两个采样器 + Bloom 参数 UBO + 曝光状态 +
/// bins SSBO。每帧按既有视图/缓冲重写（不复制任何状态）。
pub(crate) static COLOR_SCOPE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "color_scope",
    &[
        BindingDesc::new(0, DescKind::SampledImage, COMPUTE),
        BindingDesc::new(1, DescKind::Sampler, COMPUTE),
        BindingDesc::new(2, DescKind::SampledImage, COMPUTE),
        BindingDesc::new(3, DescKind::Sampler, COMPUTE),
        BindingDesc::new(4, DescKind::UniformBuffer, COMPUTE),
        BindingDesc::new(5, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(6, DescKind::SampledImage, COMPUTE),
        BindingDesc::new(7, DescKind::StorageBuffer, COMPUTE),
    ],
);

/// 采样 dispatch 的管线配方。
pub(crate) static COLOR_SCOPE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "color_scope",
    shader: &shader_abi::COLOR_SCOPE,
    entries: &[c"cs_main"],
    sets: &[&COLOR_SCOPE_SET_DESC],
    push: Some(PushRange {
        stages: COMPUTE,
        size: COLOR_SCOPE_PUSH_BYTES,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 颜色校验开关（egui「颜色」页与 BRP `color_scope_set` 写同一份）。
#[derive(bevy::prelude::Resource, Default)]
pub struct ColorScopeSettings {
    /// 显式总开关，默认关：关时整段录制跳过，命令序列与没有本模块时相同。
    pub enabled: bool,
    /// 单帧采样请求：为真时录制一帧后自动清回 `false`，不改变 `enabled`。
    pub once: bool,
}

/// 一个帧槽要绑进采样集的既有句柄（视图随 resize 换、缓冲随帧槽换，句柄不复制）。
#[derive(Clone, Copy)]
pub struct ColorScopeBindings {
    /// A 域源：present 采样的 HDR 视图。
    pub hdr: vk::ImageView,
    /// present 用的线性采样器。
    pub blit_sampler: vk::Sampler,
    /// Bloom 升采样第 0 级视图（present set2 b0 同一张）。
    pub bloom: vk::ImageView,
    /// Bloom 链自带的线性采样器。
    pub bloom_sampler: vk::Sampler,
    /// Bloom 参数 UBO（本帧槽，present 同源）。
    pub bloom_params: vk::Buffer,
    /// 曝光状态 buffer（本帧槽 `[adapted, scale, avg_log2, 1]`）。
    pub exposure_state: vk::Buffer,
    /// C 域源：tonemap 写出的 LDR 视图。
    pub ldr: vk::ImageView,
}

/// 采样 dispatch 组数（workgroup = 8×8，覆盖 stride 抽样的网格）。
#[must_use]
pub fn color_scope_groups(extent: vk::Extent2D) -> (u32, u32) {
    (
        extent.width.div_ceil(COLOR_SCOPE_STRIDE).div_ceil(8).max(1),
        extent
            .height
            .div_ceil(COLOR_SCOPE_STRIDE)
            .div_ceil(8)
            .max(1),
    )
}

/// 采样资源族：每帧槽一份 bins + 一张描述符集，池/布局随资源族存亡。
#[derive(bevy::prelude::Resource)]
pub struct ColorScopeTargets {
    device: Device,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    bins: Vec<GpuBuffer>,
    sets: Vec<vk::DescriptorSet>,
    /// 每槽集里当前写入的三张视图（缓冲/采样器稳定，只有视图随 resize 换）。
    bound: Vec<[vk::ImageView; 3]>,
    /// 已开始的采样帧计数（写进 bins 头，供 `frame_age` 对账）。
    frame_index: u64,
    /// 最近一次 [`Self::begin_frame`] 的帧槽（面板按它选 bins 回读）。
    last_slot: usize,
}

impl Drop for ColorScopeTargets {
    fn drop(&mut self) {
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.layout, None);
            }
        }
    }
}

impl ColorScopeTargets {
    /// 建 bins（每帧槽一份）与描述符布局/池/集。集要等帧侧视图才能写满，留给
    /// [`Self::bind_resources`] 首次调用。
    ///
    /// # Errors
    /// 布局、池、缓冲或描述符写失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let slots = crate::render::MAX_FRAMES_IN_FLIGHT;
        let sharing = ctx.sharing_families();
        let mut this = Self {
            device: ctx.device.clone(),
            layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            bins: Vec::with_capacity(slots),
            sets: Vec::with_capacity(slots),
            bound: vec![[vk::ImageView::null(); 3]; slots],
            frame_index: 0,
            last_slot: 0,
        };
        for _ in 0..slots {
            // bins 必须是显存里的 storage：每帧约 250 万次原子写，放宿主可见内存
            //（走 PCIe 的 system memory）会让一帧的采样从 0.3 ms 涨到几十毫秒。
            // 清零改由录制侧 `vkCmdFillBuffer` 做，头由 shader 自己写。
            let bins = GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                COLOR_SCOPE_BINS_BYTES,
                BufferRole::DeviceStorage,
                &sharing,
            )?;
            this.bins.push(bins);
        }
        this.layout = COLOR_SCOPE_SET_DESC.create(&ctx.device)?;
        let pool_sizes = merged_pool_sizes(&[(&COLOR_SCOPE_SET_DESC, slots as u32)]);
        this.pool = unsafe {
            ctx.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(slots as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )?
        };
        this.sets = unsafe {
            ctx.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.pool)
                    .set_layouts(&std::iter::repeat_n(this.layout, slots).collect::<Vec<_>>()),
            )?
        };
        Ok(this)
    }

    /// 开一帧采样：记下帧槽与本帧号。bins 的清零与头由录制侧（fill + shader）完成，
    /// 这里只推进计数器，不碰显存（bins 在 DEVICE_LOCAL）。
    ///
    /// # Errors
    /// 槽号越界。
    pub fn begin_frame(&mut self, slot: usize) -> Result<u64> {
        if slot >= self.bins.len() {
            return Err(VulkanError::Contract(format!(
                "颜色校验 bins 槽 {slot} 越界（共 {}）",
                self.bins.len()
            )));
        }
        let frame_index = self.frame_index;
        self.frame_index = self.frame_index.wrapping_add(1);
        self.last_slot = slot;
        Ok(frame_index)
    }

    /// 把本帧槽的三域视图/缓冲写进采样集（视图没变就不重写）。
    ///
    /// # Errors
    /// 槽号越界或描述符写失败。
    pub fn bind_resources(
        &mut self,
        ctx: &Context,
        slot: usize,
        b: &ColorScopeBindings,
    ) -> Result<()> {
        if slot >= self.sets.len() {
            return Err(VulkanError::Contract(format!(
                "颜色校验描述符集槽 {slot} 越界（共 {}）",
                self.sets.len()
            )));
        }
        let want = [b.hdr, b.bloom, b.ldr];
        if self.bound[slot] == want {
            return Ok(());
        }
        let mut writer = DescriptorWriter::new(&COLOR_SCOPE_SET_DESC, self.sets[slot]);
        writer
            .image(0, b.hdr, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .sampler(1, b.blit_sampler)
            .image(2, b.bloom, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .sampler(3, b.bloom_sampler)
            .buffer(4, b.bloom_params, 0, vk::WHOLE_SIZE)
            .buffer(5, b.exposure_state, 0, vk::WHOLE_SIZE)
            .image(6, b.ldr, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .buffer(7, self.bins[slot].buffer(), 0, vk::WHOLE_SIZE);
        writer.submit(&ctx.device)?;
        self.bound[slot] = want;
        Ok(())
    }

    /// 某帧槽的采样描述符集。
    #[must_use]
    pub fn set(&self, slot: usize) -> DescSet {
        DescSet::new(&COLOR_SCOPE_SET_DESC, self.sets[slot])
    }

    /// 某帧槽的 bins（回读/自检用）。
    #[must_use]
    pub fn bins(&self, slot: usize) -> &GpuBuffer {
        &self.bins[slot]
    }

    /// 采样集布局（进 [`SetLayouts`] 登记表）。
    #[must_use]
    pub fn layout(&self) -> SetLayout {
        SetLayout::new(&COLOR_SCOPE_SET_DESC, self.layout)
    }

    /// 已开始的采样帧数（BRP 算 `frame_age` 用）。
    #[must_use]
    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// 最近一帧写进 bins 的帧槽（面板/BRP 回读用）。
    #[must_use]
    pub fn last_slot(&self) -> usize {
        self.last_slot
    }

    /// 回读某帧槽的 bins（one-shot 拷贝 + fence，队列序保证看到该槽最近一个完整帧）。
    ///
    /// # Errors
    /// 槽号越界、一次性 staging/提交/映射失败或读到的总长不符。
    pub fn read_bins(&self, ctx: &Context, slot: usize) -> Result<ColorScopeSnapshot> {
        if slot >= self.bins.len() {
            return Err(VulkanError::Contract(format!(
                "颜色校验 bins 槽 {slot} 越界（共 {}）",
                self.bins.len()
            )));
        }
        let mut bytes = vec![0u8; COLOR_SCOPE_BINS_BYTES as usize];
        unsafe {
            let readback = OneShotReadback::begin(ctx, COLOR_SCOPE_BINS_BYTES)?;
            let region = vk::BufferCopy::default()
                .src_offset(0)
                .dst_offset(0)
                .size(COLOR_SCOPE_BINS_BYTES);
            ctx.device.cmd_copy_buffer(
                readback.cb,
                self.bins[slot].buffer(),
                readback.staging,
                std::slice::from_ref(&region),
            );
            readback.finish_and_read(ctx, &mut bytes)?;
        }
        ColorScopeSnapshot::from_bytes(&bytes)
    }
}

/// 三域代号：A = 原始 HDR，B = 曲线输入色，C = tonemap 后的 LDR。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScopeDomain {
    /// 原始 HDR（未加 Bloom、未乘曝光）。
    A,
    /// 曲线输入色（present 同式）。
    B,
    /// tonemap 写出的 LDR（线性 UNORM）。
    C,
}

impl ColorScopeDomain {
    /// 该域在 bins 里的起始下标（16 字节头之后；与 shader 的 BASE_A/B/C 对应）。
    #[must_use]
    pub fn base(self) -> u32 {
        COLOR_SCOPE_HEADER_U32
            + match self {
                Self::A => 0,
                Self::B => COLOR_SCOPE_DOMAIN_U32,
                Self::C => 2 * COLOR_SCOPE_DOMAIN_U32,
            }
    }
}

/// 一个域的计数五项（与 shader 的 CNT_* 对应）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorScopeCounts {
    /// 样本数。
    pub samples: u32,
    /// luma 低于轴下限（A/B：< −16 EV）。
    pub below: u32,
    /// luma 高于轴上限（A/B：≥ +8 EV）。
    pub above: u32,
    /// luma ≤ 0（A/B）/三通道全 0（C）。
    pub zero: u32,
    /// 触顶样本（C：任一通道 ≥ 1；A/B 恒 0）。
    pub clip: u32,
}

/// 一次 bins 快照（头 + 原始 u32 区），面板与 BRP 共用同一解析口径。
#[derive(Clone, Debug)]
pub struct ColorScopeSnapshot {
    /// 头 16 个 u32（magic / stride / 宽 / 高 / 帧号 / 曝光快照 4×f32 / 保留）。
    pub header: [u32; COLOR_SCOPE_HEADER_U32 as usize],
    /// 全量 bins（长度 [`COLOR_SCOPE_BINS_U32`]）。
    pub bins: Vec<u32>,
    /// 暴露给调用方的 CPU 侧帧号（回绕成 64 位，避免每 49 天跳变）。
    frame_index: u64,
}

impl ColorScopeSnapshot {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != COLOR_SCOPE_BINS_BYTES as usize {
            return Err(VulkanError::Contract(format!(
                "颜色校验 bins 读回 {} B，期望 {COLOR_SCOPE_BINS_BYTES} B",
                bytes.len()
            )));
        }
        let bins: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let header: [u32; COLOR_SCOPE_HEADER_U32 as usize] = bins
            [..COLOR_SCOPE_HEADER_U32 as usize]
            .try_into()
            .expect("bins 头长度由常量保证");
        Ok(Self {
            header,
            frame_index: u64::from(header[COLOR_SCOPE_HEADER_FRAME]),
            bins,
        })
    }

    /// 面板/BRP 判断"还没采过样"（bins 从没被写过头）。
    #[must_use]
    pub fn magic_ok(&self) -> bool {
        self.header[0] == COLOR_SCOPE_MAGIC
    }

    /// 采样分辨率。
    #[must_use]
    pub fn extent(&self) -> (u32, u32) {
        (
            self.header[COLOR_SCOPE_HEADER_WIDTH],
            self.header[COLOR_SCOPE_HEADER_HEIGHT],
        )
    }

    /// shader 写进头的帧号（回绕 32 位，最长 2^32 帧）。
    #[must_use]
    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// 头里的曝光状态快照（adapted_log2, scale, avg_log2, 1）。
    #[must_use]
    pub fn exposure_state(&self) -> [f32; 4] {
        std::array::from_fn(|i| f32::from_bits(self.header[5 + i]))
    }

    /// 某域的 luma 直方图（256 bin）。
    #[must_use]
    pub fn luma(&self, domain: ColorScopeDomain) -> &[u32] {
        let base = domain.base() as usize;
        &self.bins[base..base + 256]
    }

    /// 某域第 `ch` 通道的 parade 直方图（256 bin）。
    #[must_use]
    pub fn rgb(&self, domain: ColorScopeDomain, ch: usize) -> &[u32] {
        let base = domain.base() as usize + 256 * (ch + 1);
        &self.bins[base..base + 256]
    }

    /// 某域的计数五项。
    #[must_use]
    pub fn counts(&self, domain: ColorScopeDomain) -> ColorScopeCounts {
        let at = |i: usize| self.bins[domain.base() as usize + 1024 + i];
        ColorScopeCounts {
            samples: at(0),
            below: at(1),
            above: at(2),
            zero: at(3),
            clip: at(4),
        }
    }

    /// C 域 CbCr 散布（64×64，行 Cr、列 Cb）。
    #[must_use]
    pub fn cbcr(&self) -> &[u32] {
        let base = (COLOR_SCOPE_HEADER_U32 + 3 * COLOR_SCOPE_DOMAIN_U32) as usize;
        &self.bins[base..base + COLOR_SCOPE_CBCR_U32 as usize]
    }

    /// B 域均值的同口径期望（EV）：测光 `avg_log2` + `log2(scale)`。
    /// 头里的 `avg_log2` 是测光（乘曝光倍数之前）的读数，而 B 域已乘过 `scale`，
    /// 直接比会固定偏 `log2(scale)`——判定线⑤要按这个口径比才算"贴住测光"。
    #[must_use]
    pub fn expected_b_mean(&self) -> Option<f32> {
        let state = self.exposure_state();
        let scale = state[1];
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        Some(state[2] + bevy::math::ops::log2(scale))
    }

    /// 第 `bin` 个 A/B bin 的中心 EV（轴自检与均值用）。
    #[must_use]
    pub fn axis_bin_ev(bin: usize) -> f32 {
        let step = COLOR_SCOPE_AXIS_SPAN_EV / 256.0;
        COLOR_SCOPE_AXIS_MIN_EV + (bin as f32 + 0.5) * step
    }

    /// A/B 域按 luma 直方图算的 log2 均值（越轴样本不在 bin 里，故可能偏差）。
    #[must_use]
    pub fn mean_log2(&self, domain: ColorScopeDomain) -> Option<f32> {
        let counts = self.counts(domain);
        if counts.samples == 0 {
            return None;
        }
        let weighted: f64 = self
            .luma(domain)
            .iter()
            .enumerate()
            .filter(|&(_, &n)| n > 0)
            .map(|(i, &n)| f64::from(n) * f64::from(Self::axis_bin_ev(i)))
            .sum();
        Some((weighted / f64::from(counts.samples)) as f32)
    }

    /// 越轴比例（低于轴 / 高于轴 / 零 / 触顶），分母是样本数；无样本返回 `None`。
    #[must_use]
    pub fn ratios(&self, domain: ColorScopeDomain) -> Option<ColorScopeRatios> {
        let counts = self.counts(domain);
        if counts.samples == 0 {
            return None;
        }
        let n = counts.samples as f32;
        let ratio = |v: u32| v as f32 / n;
        Some(ColorScopeRatios {
            below: ratio(counts.below),
            above: ratio(counts.above),
            zero: ratio(counts.zero),
            clip: ratio(counts.clip),
        })
    }
}

/// 一个域的越轴/钳位比例（0…1，分母是该域样本数）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ColorScopeRatios {
    /// luma 低于轴下限的比例（A/B）。
    pub below: f32,
    /// luma 高于轴上限的比例（A/B）。
    pub above: f32,
    /// luma ≤ 0（A/B）或三通道全 0（C）的比例。
    pub zero: f32,
    /// 触顶比例（C：任一通道 ≥ 1）。
    pub clip: f32,
}

/// 回读节流 + 最近一次快照（egui「颜色」页 8 Hz 读，BRP 手动触发）。
#[derive(bevy::prelude::Resource)]
pub struct ColorScopeReader {
    interval: std::time::Duration,
    last_read: Option<std::time::Instant>,
    snapshot: Option<ColorScopeSnapshot>,
    last_error: Option<String>,
}

impl Default for ColorScopeReader {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_millis(125),
            last_read: None,
            snapshot: None,
            last_error: None,
        }
    }
}

impl ColorScopeReader {
    /// 指定节流间隔（毫秒）；面板默认 125 ms（8 Hz）。
    #[must_use]
    pub fn with_interval_ms(interval_ms: u64) -> Self {
        Self {
            interval: std::time::Duration::from_millis(interval_ms.max(1)),
            ..Self::default()
        }
    }

    /// 最近一次成功读到的快照。
    #[must_use]
    pub fn snapshot(&self) -> Option<&ColorScopeSnapshot> {
        self.snapshot.as_ref()
    }

    /// 最近一次读失败的原因（成功读后清空）。
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 到点才回读（`force` 跳过节流）。返回本次是否真的读了。
    ///
    /// # Errors
    /// bins 拷贝/映射失败；错误同时记进 [`Self::last_error`]，面板只显示不冒泡。
    pub fn refresh(
        &mut self,
        ctx: &Context,
        targets: &ColorScopeTargets,
        slot: usize,
        force: bool,
    ) -> Result<bool> {
        let now = std::time::Instant::now();
        if !force
            && let Some(last) = self.last_read
            && now.duration_since(last) < self.interval
        {
            return Ok(false);
        }
        self.last_read = Some(now);
        match targets.read_bins(ctx, slot) {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.last_error = None;
                Ok(true)
            }
            Err(err) => {
                self.last_error = Some(err.to_string());
                Err(err)
            }
        }
    }
}

/// 采样 compute 管线（进程级）。
#[derive(bevy::prelude::Resource)]
pub struct ColorScopePipelines {
    scope: ComputePipeline,
}

impl ColorScopePipelines {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            scope: ComputePipeline::new(ctx, &COLOR_SCOPE_PIPELINE, layouts)?,
        })
    }

    /// 录制句柄（Copy）。
    #[must_use]
    pub(crate) fn handles(&self) -> ComputeHandles {
        self.scope.handles()
    }
}

/// 采样 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct ColorScopeDraw {
    handles: ComputeHandles,
    set: DescSet,
    groups: (u32, u32),
    /// 本帧号（push constant，shader 写进 bins 头）。
    frame_index: u32,
    /// 本帧槽的 bins（录制侧先整份清零）。
    bins: vk::Buffer,
}

impl ColorScopeDraw {
    /// 组装一条 dispatch（`extent` 为渲染分辨率，组数由 [`color_scope_groups`] 推出）。
    #[must_use]
    pub fn new(
        handles: ComputeHandles,
        set: DescSet,
        extent: vk::Extent2D,
        frame_index: u32,
        bins: vk::Buffer,
    ) -> Self {
        Self {
            handles,
            set,
            groups: color_scope_groups(extent),
            frame_index,
            bins,
        }
    }

    /// 清零 bins → 采样 dispatch → 下游（SMAA edge 的 FS 读）可见。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        let push = self.frame_index.to_le_bytes();
        unsafe {
            device.cmd_fill_buffer(cb, self.bins, 0, COLOR_SCOPE_BINS_BYTES, 0);
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::TRANSFER,
                src_access: vk::AccessFlags::TRANSFER_WRITE,
                dst_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                dst_access: vk::AccessFlags::SHADER_WRITE,
            }
            .record(device, cb);
        }
        self.handles.record_grid(
            device,
            cb,
            &[self.set],
            &push,
            self.groups,
            Handoff::ComputeFragmentRead,
        );
        // bins 之后还会被 8 Hz 的一趟 `vkCmdCopyBuffer`（独立 command buffer、后续
        // submit）读：dispatch 的 barrier 只覆盖片元读，这里补一条 compute 写 →
        // transfer 读，否则同步校验报 READ_AFTER_WRITE。
        unsafe {
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                src_access: vk::AccessFlags::SHADER_WRITE,
                dst_stage: vk::PipelineStageFlags::TRANSFER,
                dst_access: vk::AccessFlags::TRANSFER_READ,
            }
            .record(device, cb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::bytes::put_u32s;

    #[test]
    fn domain_bases_are_after_header() {
        // 头 16 + 每域 1029：A/B/C 起始必须与 shader 的 BASE_A/B/C 相同，
        // 否则面板/BRP 会把头里的字节当直方图（9.9 联调踩过一次）。
        assert_eq!(ColorScopeDomain::A.base(), 16);
        assert_eq!(ColorScopeDomain::B.base(), 16 + COLOR_SCOPE_DOMAIN_U32);
        assert_eq!(ColorScopeDomain::C.base(), 16 + 2 * COLOR_SCOPE_DOMAIN_U32);
        let cbcr = COLOR_SCOPE_HEADER_U32 + 3 * COLOR_SCOPE_DOMAIN_U32 + COLOR_SCOPE_CBCR_U32;
        assert_eq!(cbcr, COLOR_SCOPE_BINS_U32);
    }

    #[test]
    fn bins_layout_matches_shader_constants() {
        // 与 shaders/post/color_scope.wgsl 的 HEADER_U32/DOMAIN_U32/BINS_U32 一致
        assert_eq!(COLOR_SCOPE_DOMAIN_U32, 1029);
        assert_eq!(COLOR_SCOPE_HEADER_U32, 16);
        assert_eq!(COLOR_SCOPE_BINS_U32, 7199);
        assert_eq!(COLOR_SCOPE_BINS_BYTES, 28796);
        // C 域色度区之后没有剩余
        assert_eq!(
            COLOR_SCOPE_HEADER_U32 + 3 * COLOR_SCOPE_DOMAIN_U32 + COLOR_SCOPE_CBCR_U32,
            COLOR_SCOPE_BINS_U32
        );
    }

    #[test]
    fn header_magic_matches_shader_literal() {
        // colo_scope.wgsl 的 MAGIC 字面量（0x50435343u）必须等于宿主的 "CSCP" 小端
        assert_eq!(COLOR_SCOPE_MAGIC, 0x5043_5343);
        let extent = vk::Extent2D {
            width: 1600,
            height: 900,
        };
        let raw = {
            let mut raw = [0u8; 64];
            put_u32s(
                &mut raw,
                0,
                &[
                    COLOR_SCOPE_MAGIC,
                    COLOR_SCOPE_STRIDE,
                    extent.width,
                    extent.height,
                    7,
                ],
            );
            raw
        };
        let u32_at = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
        assert_eq!(u32_at(0), COLOR_SCOPE_MAGIC);
        assert_eq!(u32_at(4), COLOR_SCOPE_STRIDE);
        assert_eq!(u32_at(8), 1600);
        assert_eq!(u32_at(12), 900);
        assert_eq!(u32_at(16), 7);
        // 曝光快照由 shader 覆写下标 5..9，CPU 侧留 0
        assert_eq!(u32_at(20), 0);
    }

    #[test]
    fn groups_cover_stride_grid() {
        let (gx, gy) = color_scope_groups(vk::Extent2D {
            width: 1600,
            height: 900,
        });
        assert_eq!((gx, gy), (50, 29));
        // 小尺寸也至少发一组
        assert_eq!(
            color_scope_groups(vk::Extent2D {
                width: 1,
                height: 1
            }),
            (1, 1)
        );
    }
}
