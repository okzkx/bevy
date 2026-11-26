//! 自动曝光（眼适应）：**独立的显示段模块**，不与 SCGI/IBL 的物理量耦合。
//!
//! 数据流：HDR 中间目标（合成写完、已转 SHADER_READ_ONLY）→ `cs_measure` 采 64×36
//! tile 的 log2 亮度均值 → `cs_adapt` 做时域适应并算出全帧曝光倍数 → present tonemap
//! 在色调映射前乘上它。`BloomSettings::gain` 仍是曲线亮度增益，两者相乘不冲突；
//! `common/brdf.wgsl` 的 `EXPOSURE` 锚点不动（它决定 HDR 的绝对刻度，本模块只在其上
//! 做显示段补偿）。
//!
//! 开关语义：`ExposureSettings::mode == Manual` 时 `cs_adapt` 把倍数写 1.0，画面与
//! 没有本模块逐像素相同（A/B 对照、能量回归都跑这个模式）；`Auto` 才按适应值缩放。
//! 手动模式下仍会更新 `adapted`（用于切回 Auto 时不从黑屏开始爬），所以两种模式
//! 之间切换不需要额外初始化。
//!
//! 时域状态与帧槽绑定：`state` 缓冲每帧槽一份，避免相邻两帧同时读改写同一份；
//! 代价是同一槽两帧才更新一次，shader 的 `rate_*` 直接按"每次更新"定义。
//!
//! 参考实现：Unity HDRP 的 `Exposure`（`Runtime/RenderPipeline` 的 eye adaptation，
//! 同样走"log 平均亮度 → 时域适应 → 曝光倍数"，但挂在专用历史纹理上）。本模块按
//! 本仓库"每帧槽各一份 state"的既有纪律重写，不复用其纹理历史。

use ash::{vk, Device};

use crate::common::bytes::{put_f32, put_u32s};
use crate::common::error::Result;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::{
    merged_pool_sizes, stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts,
};
use crate::render::core::binding::shader_abi;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::Context;

/// 测光网格（列 / 行）：64×36 = 2304 tile，每 tile 4×4 采样点。
pub const EXPOSURE_TILES_X: u32 = 64;
/// 测光网格行数。
pub const EXPOSURE_TILES_Y: u32 = 36;
/// tile 总数（`measures` 缓冲的 f32 个数、测光 dispatch 的线程数）。
pub const EXPOSURE_TILE_COUNT: u32 = EXPOSURE_TILES_X * EXPOSURE_TILES_Y;
/// 参数 UBO 字节长（10×f32 + 垫，std140 对齐 16）。
pub const EXPOSURE_PARAMS_SIZE: u32 = 48;

/// `measures` 缓冲字节长。
const MEASURE_BYTES: u64 = EXPOSURE_TILE_COUNT as u64 * 4;
/// `state` 缓冲字节长（一个 vec4f）。
const STATE_BYTES: u64 = 16;

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 测光 + 适应共用的描述符集：b0 参数 UBO / b1 HDR / b2 tile 均值 / b3 适应状态。
pub(crate) static EXPOSURE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "exposure",
    &[
        BindingDesc::new(0, DescKind::UniformBuffer, COMPUTE),
        BindingDesc::new(1, DescKind::SampledImage, COMPUTE),
        BindingDesc::new(2, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(3, DescKind::StorageBuffer, COMPUTE),
    ],
);

/// 只给 tonemap 读适应结果（b0 = state 的 storage buffer）。
pub(crate) static EXPOSURE_STATE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "exposure_state",
    &[BindingDesc::new(
        0,
        DescKind::StorageBuffer,
        stages(&[vk::ShaderStageFlags::FRAGMENT]),
    )],
);

/// 测光 dispatch（每线程一个 tile）。
pub(crate) static EXPOSURE_MEASURE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "exposure_measure",
    shader: &shader_abi::EXPOSURE,
    entries: &[c"cs_measure"],
    sets: &[&EXPOSURE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 适应 dispatch（单线程：汇总 + 时域 + 求倍数）。
pub(crate) static EXPOSURE_ADAPT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "exposure_adapt",
    shader: &shader_abi::EXPOSURE,
    entries: &[c"cs_adapt"],
    sets: &[&EXPOSURE_SET_DESC],
    push: None,
    color_formats: &[],
    depth_format: None,
};

/// 曝光模式：手动（倍数恒 1）/ 自动（眼适应）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExposureMode {
    /// 手动：旁路本模块，画面与没有它逐像素相同。
    Manual,
    /// 自动：按帧平均亮度做眼适应。
    #[default]
    Auto,
}

impl ExposureMode {
    /// UBO 标量（`ExposureParams.mode` 偏移 0）。
    #[must_use]
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Manual => 0,
            Self::Auto => 1,
        }
    }
}

/// 自动曝光参数（BRP `exposure_set` 写，draw_frame 读）。
#[derive(bevy::prelude::Resource)]
pub struct ExposureSettings {
    /// 手动 / 自动。
    pub mode: ExposureMode,
    /// 目标中灰（线性亮度口径）：帧平均亮度等于它时倍数 = 1。
    pub key: f32,
    /// 曝光倍数下限（避免全黑场景把噪声放大到满屏）。
    pub min_scale: f32,
    /// 曝光倍数上限（避免强光场景把画面压死）。
    pub max_scale: f32,
    /// 画面变暗时的适应速率（1/s，即"提亮"方向）。
    pub rate_brighten: f32,
    /// 画面变亮时的适应速率（1/s，即"压暗"方向）。
    pub rate_darken: f32,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        // key 的口径：HDR 帧几何平均亮度等于它时倍数 = 1。9.9.9 重新标定：默认机位
        // 今天实测 `avg_log2 = -6.261`（`exposure_get` 的 `avg_log2`，且 `color_scope_get`
        // 的 A 域均值与它差 0.002 EV），2^-6.261 ≈ 0.01305——定在这里，默认机位的倍数
        // 就是 1（画面与没有本模块时一致），只有更暗的场景才被提亮。
        //
        // 9.9.9 之前 key=0.01（按旧基线 -6.65 标的），默认机位实际倍数 0.767、偏暗；
        // 判定线⑭要求室内倍数落在 0.9–1.1，故改到实测中灰。
        //
        // 不能照抄 HDRP 的 0.18：本管线 tonemap 前还有 `BloomSettings::gain`
        // （ACES 亮度参数），两者叠乘。key 提到 0.01305 后 gain 同步从 8.0 降到 6.13，
        // 保持 key × gain = 0.08 不变——未触下限的场景输入曲线不变，室内基线 C 域
        // p50 与旧参数逐位同级（9.9.9 实测差 0.00%）。
        //
        // 上限 4× 的理由：默认太阳（太阳不进大厅）实测 avg_log2 ≈ -7.9，需要约 3.1×
        // 才能达到同等的可读性；4× 留一点余量给更暗的机位，同时不会把拱廊外的高光
        // 压到死白。
        //
        // 下限 0.05（9.9.9 由 0.25 下调）：纯天空实测 avg_log2 ≈ -2.87，要让它落到
        // 中灰需要 0.01305/2^-2.87 ≈ 0.096 的压暗倍数；0.25 会把天空钳在比目标亮
        // 2.6× 的位置（判定线⑬的"整体白"）。0.05 给正午天顶/日落米氏峰留 2× 余量，
        // 同时不影响室内（室内需要的 0.77/1.0 远在下限之上）。
        Self {
            mode: ExposureMode::Auto,
            key: 0.01305,
            min_scale: 0.05,
            max_scale: 4.0,
            rate_brighten: 0.7,
            rate_darken: 3.0,
        }
    }
}

/// 参数 UBO 宿主侧取数（`pack_exposure_params` 排布）。
pub struct ExposureParamsData {
    /// 模式（见 [`ExposureMode::as_u32`]）。
    pub mode: u32,
    /// 目标中灰。
    pub key: f32,
    /// 倍数下限 / 上限。
    pub min_scale: f32,
    pub max_scale: f32,
    /// 适应速率（提亮 / 压暗，1/s）。
    pub rate_brighten: f32,
    pub rate_darken: f32,
    /// 本次适应的时间步长（秒）。
    pub dt: f32,
}

/// 参数打包（偏移按 `shaders/post/exposure.wgsl` 的 `ExposureParams`）。
#[must_use]
pub fn pack_exposure_params(d: &ExposureParamsData) -> [u8; EXPOSURE_PARAMS_SIZE as usize] {
    let mut out = [0u8; EXPOSURE_PARAMS_SIZE as usize];
    put_u32s(
        &mut out,
        0,
        &[d.mode, EXPOSURE_TILES_X, EXPOSURE_TILES_Y, 0],
    );
    put_f32(&mut out, 16, d.key);
    put_f32(&mut out, 20, d.min_scale);
    put_f32(&mut out, 24, d.max_scale);
    put_f32(&mut out, 28, d.rate_brighten);
    put_f32(&mut out, 32, d.rate_darken);
    put_f32(&mut out, 36, d.dt);
    out
}

/// 中性状态字节（适应值 0、倍数 1、均值 0、已初始化位 1）。
fn neutral_state_bytes() -> [u8; STATE_BYTES as usize] {
    let mut raw = [0u8; STATE_BYTES as usize];
    for (i, value) in [0.0f32, 1.0, 0.0, 1.0].into_iter().enumerate() {
        raw[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    raw
}

/// 测光 dispatch 组数（workgroup = 64 线程）。
#[must_use]
pub fn measure_groups() -> u32 {
    EXPOSURE_TILE_COUNT.div_ceil(64)
}

/// 曝光资源族：参数 UBO ×帧槽 + tile 均值 ×帧槽 + 适应状态 ×帧槽 + 两套 set。
#[derive(bevy::prelude::Resource)]
pub struct ExposureTargets {
    device: Device,
    params_layout: vk::DescriptorSetLayout,
    state_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    params: Vec<GpuBuffer>,
    measures: Vec<GpuBuffer>,
    states: Vec<GpuBuffer>,
    sets: Vec<vk::DescriptorSet>,
    state_sets: Vec<vk::DescriptorSet>,
    /// 恒定中性状态（倍数 1）：调试显示模式绑它，既不测光也不消耗适应历史。
    neutral: GpuBuffer,
    neutral_set: vk::DescriptorSet,
    /// 每槽描述符集里当前写入的 HDR 视图（帧槽各自一张 HDR，resize 后都要重写）。
    bound_hdr: Vec<vk::ImageView>,
}

impl Drop for ExposureTargets {
    fn drop(&mut self) {
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.params_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.params_layout, None);
            }
            if self.state_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.state_layout, None);
            }
        }
    }
}

impl ExposureTargets {
    /// 建参数 / 测光 / 状态三类缓冲、两套布局与描述符集。
    ///
    /// # Errors
    /// 布局、池、缓冲或描述符写失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let slots = crate::render::MAX_FRAMES_IN_FLIGHT;
        let sharing = ctx.sharing_families();
        // 中性状态：倍数 1、已初始化。调试显示模式绑它，不参与适应也不改各槽历史。
        let mut neutral = GpuBuffer::create_with_families(
            &ctx.device,
            ctx.memory_contract(),
            STATE_BYTES,
            BufferRole::Storage,
            &sharing,
        )?;
        neutral.write(0, &neutral_state_bytes())?;
        let mut this = Self {
            device: ctx.device.clone(),
            params_layout: vk::DescriptorSetLayout::null(),
            state_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            params: Vec::new(),
            measures: Vec::new(),
            states: Vec::new(),
            sets: Vec::new(),
            state_sets: Vec::new(),
            neutral,
            neutral_set: vk::DescriptorSet::null(),
            bound_hdr: vec![vk::ImageView::null(); slots],
        };
        // 失败路径：已建句柄都在字段里，由 Drop 回收
        for _ in 0..slots {
            this.params.push(GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                u64::from(EXPOSURE_PARAMS_SIZE),
                BufferRole::FrameUniform,
                &sharing,
            )?);
            this.measures.push(GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                MEASURE_BYTES,
                BufferRole::Storage,
                &sharing,
            )?);
            // 首帧 w = 0（未初始化）→ cs_adapt 直接吸附本帧均值，不从 0 爬。
            let mut state = GpuBuffer::create_with_families(
                &ctx.device,
                ctx.memory_contract(),
                STATE_BYTES,
                BufferRole::Storage,
                &sharing,
            )?;
            state.write(0, &[0u8; STATE_BYTES as usize])?;
            this.states.push(state);
        }
        this.params_layout = EXPOSURE_SET_DESC.create(&ctx.device)?;
        this.state_layout = EXPOSURE_STATE_SET_DESC.create(&ctx.device)?;
        let pool_sizes = merged_pool_sizes(&[
            (&EXPOSURE_SET_DESC, slots as u32),
            // 状态集比帧槽多一个：末位留给恒定中性状态（调试显示模式）
            (&EXPOSURE_STATE_SET_DESC, slots as u32 + 1),
        ]);
        this.pool = unsafe {
            ctx.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets((2 * slots + 1) as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )?
        };
        let alloc =
            |layout: vk::DescriptorSetLayout, count: usize| -> Result<Vec<vk::DescriptorSet>> {
                unsafe {
                    Ok(ctx.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(this.pool)
                            .set_layouts(&std::iter::repeat_n(layout, count).collect::<Vec<_>>()),
                    )?)
                }
            };
        this.sets = alloc(this.params_layout, slots)?;
        let mut state_sets = alloc(this.state_layout, slots + 1)?;
        this.neutral_set = state_sets.pop().expect("中性状态集缺失");
        this.state_sets = state_sets;
        // 测光集含 b1 = HDR，要等目标视图存在才能写满（描述符集要求声明里的 binding
        // 全覆盖，见 DescriptorWriter::coverage_problems），所以留给 `bind_hdr` 首帧写。
        // tonemap 只读 state，不依赖 HDR，现在就写满。
        for slot in 0..slots {
            let mut state_writer =
                DescriptorWriter::new(&EXPOSURE_STATE_SET_DESC, this.state_sets[slot]);
            state_writer.buffer(0, this.states[slot].buffer(), 0, vk::WHOLE_SIZE);
            state_writer.submit(&ctx.device)?;
        }
        let mut neutral_writer = DescriptorWriter::new(&EXPOSURE_STATE_SET_DESC, this.neutral_set);
        neutral_writer.buffer(0, this.neutral.buffer(), 0, vk::WHOLE_SIZE);
        neutral_writer.submit(&ctx.device)?;
        Ok(this)
    }

    /// 把该帧槽的 HDR 视图写进它的测光描述符集（视图变了才重写）。
    ///
    /// # Errors
    /// 槽号越界或描述符写失败。
    pub fn bind_hdr(&mut self, ctx: &Context, slot: usize, view: vk::ImageView) -> Result<()> {
        if slot >= self.sets.len() {
            return Err(crate::common::error::VulkanError::Contract(format!(
                "曝光描述符集槽 {slot} 越界"
            )));
        }
        if self.bound_hdr[slot] == view {
            return Ok(());
        }
        let mut writer = DescriptorWriter::new(&EXPOSURE_SET_DESC, self.sets[slot]);
        writer
            .buffer(
                0,
                self.params[slot].buffer(),
                0,
                u64::from(EXPOSURE_PARAMS_SIZE),
            )
            .image(1, view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .buffer(2, self.measures[slot].buffer(), 0, vk::WHOLE_SIZE)
            .buffer(3, self.states[slot].buffer(), 0, vk::WHOLE_SIZE);
        writer.submit(&ctx.device)?;
        self.bound_hdr[slot] = view;
        Ok(())
    }

    /// 写某帧槽的参数 UBO。
    ///
    /// # Errors
    /// 缓冲写失败（映射失效等）。
    pub fn write_params(&mut self, slot: usize, data: &ExposureParamsData) -> Result<()> {
        self.params[slot].write(0, &pack_exposure_params(data))
    }

    /// 调试显示模式的 tonemap set：恒定倍数 1，不读写任何帧槽状态，因此不会打断
    /// 正常渲染模式的适应历史（切回 Lambert 后曝光从上次的值继续，不会重新爬）。
    #[must_use]
    pub fn neutral_state_set(&self) -> DescSet {
        DescSet::new(&EXPOSURE_STATE_SET_DESC, self.neutral_set)
    }

    /// 某帧槽的适应状态 buffer 句柄（颜色校验采样集与 tonemap 同源）。
    #[must_use]
    pub fn state_buffer(&self, slot: usize) -> vk::Buffer {
        self.states[slot].buffer()
    }

    /// 读某帧槽的适应状态（`(adapted_log2, scale, avg_log2, 1)`），供 BRP 自检。
    pub fn read_state(&mut self, slot: usize) -> Result<[f32; 4]> {
        let mut raw = [0u8; STATE_BYTES as usize];
        self.states[slot].read(0, &mut raw)?;
        Ok([
            f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            f32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
            f32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
            f32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]),
        ])
    }

    /// 测光 dispatch 的描述符集。
    #[must_use]
    pub fn set(&self, slot: usize) -> DescSet {
        DescSet::new(&EXPOSURE_SET_DESC, self.sets[slot])
    }

    /// tonemap 读适应结果的描述符集。
    #[must_use]
    pub fn state_set(&self, slot: usize) -> DescSet {
        DescSet::new(&EXPOSURE_STATE_SET_DESC, self.state_sets[slot])
    }

    /// 两套布局句柄（进 [`SetLayouts`] 登记表）。
    #[must_use]
    pub fn layouts(&self) -> [SetLayout; 2] {
        [
            SetLayout::new(&EXPOSURE_SET_DESC, self.params_layout),
            SetLayout::new(&EXPOSURE_STATE_SET_DESC, self.state_layout),
        ]
    }
}

/// 两条曝光管线（进程级）。
#[derive(bevy::prelude::Resource)]
pub struct ExposurePipelines {
    measure: ComputePipeline,
    adapt: ComputePipeline,
}

impl ExposurePipelines {
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            measure: ComputePipeline::new(ctx, &EXPOSURE_MEASURE_PIPELINE, layouts)?,
            adapt: ComputePipeline::new(ctx, &EXPOSURE_ADAPT_PIPELINE, layouts)?,
        })
    }

    /// 测光 dispatch 句柄。
    #[must_use]
    pub(crate) fn measure(&self) -> ComputeHandles {
        self.measure.handles()
    }

    /// 适应 dispatch 句柄。
    #[must_use]
    pub(crate) fn adapt(&self) -> ComputeHandles {
        self.adapt.handles()
    }
}

/// 一帧的曝光录制输入：两条 dispatch + 共用描述符集 + tonemap 的 state 集。
#[derive(Clone, Copy)]
pub struct ExposureDraw {
    /// 测光（每线程一个 tile）。
    pub measure: ComputeHandles,
    /// 适应（单线程）。
    pub adapt: ComputeHandles,
    /// 两条 dispatch 共用（参数 / HDR / tile 均值 / 状态）。
    pub set: DescSet,
    /// tonemap 读 `state[0].y` 作曝光倍数。
    pub state_set: DescSet,
}

impl ExposureDraw {
    /// 录测光 → 适应两条 dispatch。交接屏障：测光写 → 适应读（compute→compute），
    /// 适应写 → tonemap 读（compute→fragment）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.measure.record(
            device,
            cb,
            &[self.set],
            &[],
            measure_groups(),
            Handoff::ComputeRead,
        );
        self.adapt.record(
            device,
            cb,
            &[self.set],
            &[],
            1,
            Handoff::ComputeFragmentRead,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_grid_covers_configured_tiles() {
        assert_eq!(EXPOSURE_TILES_X * EXPOSURE_TILES_Y, EXPOSURE_TILE_COUNT);
        // 64 线程一组，最后一组不满也要发
        assert_eq!(measure_groups(), EXPOSURE_TILE_COUNT.div_ceil(64));
        assert_eq!(measure_groups(), 36);
    }

    #[test]
    fn params_layout_matches_shader_offsets() {
        let packed = pack_exposure_params(&ExposureParamsData {
            mode: 1,
            key: 0.18,
            min_scale: 0.5,
            max_scale: 6.0,
            rate_brighten: 0.7,
            rate_darken: 3.0,
            dt: 0.016,
        });
        assert_eq!(packed.len(), EXPOSURE_PARAMS_SIZE as usize);
        let u32_at =
            |o: usize| u32::from_le_bytes([packed[o], packed[o + 1], packed[o + 2], packed[o + 3]]);
        let f32_at =
            |o: usize| f32::from_le_bytes([packed[o], packed[o + 1], packed[o + 2], packed[o + 3]]);
        assert_eq!(u32_at(0), 1);
        assert_eq!(u32_at(4), EXPOSURE_TILES_X);
        assert_eq!(u32_at(8), EXPOSURE_TILES_Y);
        assert!((f32_at(16) - 0.18).abs() < 1e-6);
        assert!((f32_at(20) - 0.5).abs() < 1e-6);
        assert!((f32_at(24) - 6.0).abs() < 1e-6);
        assert!((f32_at(28) - 0.7).abs() < 1e-6);
        assert!((f32_at(32) - 3.0).abs() < 1e-6);
        assert!((f32_at(36) - 0.016).abs() < 1e-6);
    }

    /// 曝光倍数的口径：帧均值 = key 时倍数 1（手写一遍 shader 的公式，防止两边跑偏）。
    #[allow(clippy::disallowed_methods)] // 测试夹具重演 shader 公式，非渲染热路径
    fn scale_for(avg_log2: f32, key: f32, min: f32, max: f32) -> f32 {
        (key / avg_log2.exp2()).clamp(min, max)
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // 测试夹具重演 shader 公式，非渲染热路径
    fn scale_is_one_at_key_and_clamped() {
        assert!((scale_for(0.18f32.log2(), 0.18, 0.5, 6.0) - 1.0).abs() < 1e-5);
        // 更暗的帧 → 倍数更大；更亮的帧 → 倍数更小
        assert!(scale_for(-6.0, 0.18, 0.5, 6.0) > scale_for(-2.0, 0.18, 0.5, 6.0));
        assert!((scale_for(-30.0, 0.18, 0.5, 6.0) - 6.0).abs() < 1e-6);
        assert!((scale_for(30.0, 0.18, 0.5, 6.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn default_key_matches_accepted_baseline_exposure() {
        let s = ExposureSettings::default();
        // 标定依据（9.9.9 重标）：默认机位实测 avg_log2 = -6.261（`exposure_get`，且
        // 颜色页 A 域均值与它差 0.002 EV）。默认 key 必须让这个场景倍数≈1，否则等于
        // 悄悄改动了验收过的画面亮度（判定线⑭：0.9–1.1）。
        let accepted = scale_for(-6.261, s.key, s.min_scale, s.max_scale);
        assert!(
            (accepted - 1.0).abs() < 0.03,
            "默认 key 应让默认机位倍数≈1，实测 {accepted}"
        );
        // 默认太阳（太阳不进大厅）实测 avg_log2 ≈ -7.9：应提亮 2.5–3.5×，且不顶到上限
        let default_sun = scale_for(-7.9, s.key, s.min_scale, s.max_scale);
        assert!(
            (2.5..3.5).contains(&default_sun),
            "默认太阳应落在 2.5–3.5×，实测 {default_sun}"
        );
        // 纯天空（9.9.9 基线 avg_log2 ≈ -2.87）：下限不钳制，倍数落在 0.05–0.2
        let sky = scale_for(-2.873, s.key, s.min_scale, s.max_scale);
        assert!(
            (0.05..0.2).contains(&sky),
            "纯天空应能压暗到 0.05–0.2×（下限 0.05 不钳制），实测 {sky}"
        );
    }

    #[test]
    fn default_settings_are_auto_and_bounded() {
        let s = ExposureSettings::default();
        assert_eq!(s.mode, ExposureMode::Auto);
        assert!(s.min_scale > 0.0 && s.min_scale < 1.0);
        assert!(s.max_scale > 1.0);
        // 暗适应比亮适应慢（人眼；也让"走进暗处"不会瞬间跳亮）
        assert!(s.rate_brighten < s.rate_darken);
        assert_eq!(ExposureMode::Manual.as_u32(), 0);
        assert_eq!(ExposureMode::Auto.as_u32(), 1);
    }
}
