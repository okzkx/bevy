//! GPU LOD 选档的 compute 段：管线、push 排布、CPU 参照纯函数、诊断账面。在遮挡
//! 测试之后、indirect 组装之前运行：本 pass 写档位环 b8，build_indirect 读它，命令
//! 三字段从 LOD 变体表按档直址，instanceCount 语义不变。
//!
//! 机制见 lod_select.wgsl 头注，这里只记 Rust 侧约定：
//! - **判据**：屏幕覆盖尺寸（AABB 投屏 clamp 后最大边）。自然档位 =
//!   floor(log2(threshold / s))，阈值/迟滞随 push 每帧进 compute。
//! - **迟滞**：向粗切换需越过自然边界 ×(1−h)，向细切换即时；prev 读档位环本槽 =
//!   两帧前本帧槽输出（host 读同样在 fence 之后），2 帧滞后由迟滞带吸收，静态场景
//!   收敛值不受影响。
//! - **无跨帧真值**：选档每帧从当前相机现算，档位环只是迟滞状态；未驻留档由
//!   build_indirect 的表内回退承担。
//! - **开关**（[`LodSettings`]）：关 = 不 dispatch，档位环 host 填 0（最细档），命令
//!   三字段回到对象记录基准。
//! - dispatch 上界 = 实例槽 bump bound。

use ash::vk;
use bevy::math::Mat4;
use bevy::prelude::Resource;

use crate::common::bytes::{put_f32, put_f32s, put_mat4, put_u32};
use crate::common::error::Result;

use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::Context;
use crate::render::MAX_LOD_LEVELS;

pub(crate) static LOD_SELECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "lod_select",
    shader: &shader_abi::LOD_SELECT,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC],
    push: Some(PushRange { stages: vk::ShaderStageFlags::COMPUTE, size: LOD_PUSH_SIZE }),
    color_formats: &[],
    depth_format: None,
};

/// compute push 总长（layout 声明 + WGSL `PushLod` 同源）：
/// mat4（64B）+ screen（8B @64）+ bound（u32 @72）+ threshold（f32 @76）+
/// hysteresis（f32 @80）+ 垫（到 16 对齐）= 96B。规范保证 maxPushConstantsSize
/// ≥ 128，无需查询。
const LOD_PUSH_SIZE: u32 = 96;

/// [`pack_lod_push`] 的 WGSL 布局镜像（与 `lod_select.wgsl::PushLod` 对照）。
#[repr(C)]
struct LodPushLayout {
    view_proj: [[f32; 4]; 4],
    screen: [f32; 2],
    bound: u32,
    threshold: f32,
    hysteresis: f32,
    _pad: [u32; 3],
}

const _: () = {
    assert!(std::mem::offset_of!(LodPushLayout, screen) == 64);
    assert!(std::mem::offset_of!(LodPushLayout, bound) == 72);
    assert!(std::mem::offset_of!(LodPushLayout, threshold) == 76);
    assert!(std::mem::offset_of!(LodPushLayout, hysteresis) == 80);
    assert!(std::mem::size_of::<LodPushLayout>() as u32 == LOD_PUSH_SIZE);
};

/// view_proj + 屏幕尺寸 + dispatch 上界 + 判据参数 → 96B compute push 字节
///（偏移按 [`LodPushLayout`] 静态断言表排布）。
#[must_use]
fn pack_lod_push(
    view_proj: &Mat4,
    screen: [f32; 2],
    bound: u32,
    threshold_px: f32,
    hysteresis: f32,
) -> [u8; LOD_PUSH_SIZE as usize] {
    let mut out = [0u8; LOD_PUSH_SIZE as usize];
    put_mat4(&mut out, 0, view_proj);
    put_f32s(&mut out, 64, &screen);
    put_u32(&mut out, 72, bound);
    put_f32(&mut out, 76, threshold_px);
    put_f32(&mut out, 80, hysteresis);
    out
}

// ===== CPU 参照纯函数（与 lod_select.wgsl 同式，库测试与 BRP 对照用）=====

/// 屏幕覆盖尺寸 → 自然档位：`floor(log2(threshold / s))`，s 每减半升一档，
/// clamp 进 `[0, max_levels-1]`（s ≥ 阈值 → 0 最细档；完全离屏 s=0 → 最粗档，
/// 反正不画）。
#[must_use]
pub fn lod_raw_level(size_px: f32, threshold_px: f32, max_levels: u32) -> u32 {
    let s = size_px.max(1.0);
    let raw = bevy::math::ops::log2(threshold_px.max(1.0) / s).floor();
    (raw as i64).clamp(0, i64::from(max_levels) - 1) as u32
}

/// 迟滞档位裁决：向粗（raw > prev）需 `s ≤ 该档自然边界 ×(1−h)`（越过阈值带才
/// 降档），向细（raw < prev）即时升档，相等保持。prev = 档位环本槽值。
#[must_use]
pub fn lod_hysteresis_level(
    raw: u32,
    prev: u32,
    size_px: f32,
    threshold_px: f32,
    hysteresis: f32,
    max_levels: u32,
) -> u32 {
    let prev = prev.min(max_levels - 1);
    if raw > prev {
        let boundary = threshold_px * bevy::math::ops::exp2(-(raw as f32));
        if size_px <= boundary * (1.0 - hysteresis) {
            raw
        } else {
            prev
        }
    } else {
        // 变细（raw < prev）与持平（raw == prev）都即时生效
        raw
    }
}

/// LOD 开关与判据参数（egui checkbox 与 BRP `lod_set_enabled`/`lod_set_params`
/// 写，draw_frame 读）。关 = 不 dispatch 选档，档位环 host 填 0。
#[derive(Resource)]
pub struct LodSettings {
    pub enabled: bool,
    /// 档位阈值（像素）：屏幕覆盖 ≥ 此值 = 最细档 0。
    pub threshold_px: f32,
    /// 迟滞系数 h ∈ [0,1)：向粗切换需越过自然边界 ×(1−h)。
    pub hysteresis: f32,
}

impl Default for LodSettings {
    fn default() -> Self {
        Self { enabled: true, threshold_px: 256.0, hysteresis: 0.25 }
    }
}

/// LOD 诊断读数（draw_frame 每帧刷新，egui 面板/BRP 读）。只做计量，不得用它决定
/// 画什么。档位读数 = 本帧槽两帧前 compute 的输出（host 直读，在 fence 之后）。
#[derive(Resource, Default)]
pub struct LodDiagnostics {
    /// LOD 开关快照。
    pub enabled: bool,
    /// 本帧是否实际 dispatch（enabled ∧ 场内有实例槽）。
    pub dispatched: bool,
    /// dispatch 上界（实例槽 bump bound）。
    pub bound: u32,
    /// 档位分布（在场可绘行中各档行数）。
    pub level_counts: [u32; MAX_LOD_LEVELS as usize],
    /// 参与统计的行数（= 本帧 DrawList 可绘行数）。
    pub rows_compared: usize,
    /// 读数所属帧槽。
    pub marks_frame: usize,
}

/// 一次选档 dispatch 的录制输入（draw_frame 组装，`record` 消费）。
#[derive(Clone, Copy)]
pub struct LodDraw {
    pub pass: ComputeHandles,
    /// 96B push：view_proj + screen + bound + threshold + hysteresis。
    pub push: [u8; LOD_PUSH_SIZE as usize],
    /// dispatch 组数 = bound 取整到 workgroup（64）。
    pub groups: u32,
}

/// 选档 compute 管线（instance_set 单集 + 96B push）。
#[derive(Resource)]
pub struct LodPass {
    pipeline: ComputePipeline,
}

impl LodPass {
    /// 建管线：layout 只借 instance_set（读 b0/b1、写 b8）。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self { pipeline: ComputePipeline::new(ctx, &LOD_SELECT_PIPELINE, layouts)? })
    }

    /// 组一帧 dispatch 输入。draw_frame 消费，录制走 [`record`]。
    #[must_use]
    pub fn draw(
        &self,
        view_proj: &Mat4,
        screen: [f32; 2],
        bound: u32,
        threshold_px: f32,
        hysteresis: f32,
    ) -> LodDraw {
        LodDraw {
            pass: self.pipeline.handles(),
            push: pack_lod_push(view_proj, screen, bound, threshold_px, hysteresis),
            groups: bound.div_ceil(64),
        }
    }
}

impl LodDraw {
    /// 录制一帧选档 dispatch（遮挡测试之后、build_indirect 之前）。交接屏障罩住档位环
    /// 写 → build_indirect 读。
    pub(crate) fn record(&self, device: &ash::Device, command_buffer: vk::CommandBuffer, instance_set: DescSet) {
        self.pass.record(device, command_buffer, &[instance_set], &self.push, self.groups, Handoff::VertexComputeRead);
    }
}

#[cfg(test)]
mod tests {
    use super::{lod_hysteresis_level, lod_raw_level, pack_lod_push, LOD_PUSH_SIZE};
    use super::MAX_LOD_LEVELS as MAX_LEVELS;
    use bevy::math::{Mat4, Vec3};

    /// 自然档位分带：floor 语义下 s 每减半升一档，边界值归更粗档（s = T/2 恰为档 1）。
    #[test]
    fn raw_level_bands() {
        let t = 256.0f32;
        let max = MAX_LEVELS;
        assert_eq!(lod_raw_level(t, t, max), 0, "s = 阈值 → 最细档");
        assert_eq!(lod_raw_level(t * 2.0, t, max), 0, "s > 阈值 → 最细档");
        assert_eq!(lod_raw_level(t * 0.99, t, max), 0, "s 略低于阈值仍在档 0（floor 不跳档）");
        assert_eq!(lod_raw_level(t / 2.0, t, max), 1, "s = T/2 → 档 1");
        assert_eq!(lod_raw_level(t / 3.9, t, max), 1, "s ∈ (T/4, T/2] → 档 1");
        assert_eq!(lod_raw_level(t / 4.0, t, max), 2, "s = T/4 → 档 2");
        assert_eq!(lod_raw_level(t / 8.0, t, max), 3, "s = T/8 → 档 3");
        assert_eq!(lod_raw_level(0.0, t, max), 3, "完全离屏 → 最粗档（clamp）");
        // 档位上限 clamp：s 再小不越 MAX
        assert_eq!(lod_raw_level(0.001, t, max), 3);
    }

    /// 迟滞状态机：向粗需越过 (1−h) 边界、向细即时、带内不抖动。
    #[test]
    fn hysteresis_state_machine() {
        let t = 256.0f32;
        let h = 0.25f32;
        let max = MAX_LEVELS;
        // 静止：raw == prev → 原值
        assert_eq!(lod_hysteresis_level(1, 1, t / 2.0, t, h, max), 1);
        // 向细（raw < prev）即时升档——相机靠近立刻变细
        assert_eq!(lod_hysteresis_level(0, 2, t, t, h, max), 0);
        // 向粗（raw > prev）：s 在边界与 (1−h) 边界之间 → 留 prev（迟滞带）
        // s = 0.9·B(1) = 0.5T·0.8 = 0.4T：(1−h)·B(1) = 0.3T < s → 不降
        assert_eq!(lod_hysteresis_level(1, 0, t * 0.4, t, h, max), 0, "迟滞带内保持细档");
        // s ≤ (1−h)·边界 → 降档（s = 0.3T ≤ 0.375T）
        assert_eq!(lod_hysteresis_level(1, 0, t * 0.3, t, h, max), 1);
        // 往返无抖动：降档后 s 回升到迟滞带内仍保持粗档（向细即时，但 raw
        // 尚未越过更细档的自然边界——s=0.4T 的 raw=1 == prev）
        assert_eq!(lod_hysteresis_level(1, 1, t * 0.4, t, h, max), 1);
        // prev 越界（环内容异常）clamp 进范围——不 panic 不放大
        assert_eq!(lod_hysteresis_level(0, 99, t, t, h, max), 0);
    }

    /// push 打包与布局断言一致（96B：矩阵 @0..64、screen @64、bound @72、
    /// threshold @76、hysteresis @80、垫保持 0）。
    #[test]
    fn pack_lod_push_layout() {
        let vp = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let bytes = pack_lod_push(&vp, [1280.0, 720.0], 77, 256.0, 0.25);
        assert_eq!(bytes.len(), LOD_PUSH_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(0), 1.0, "mat4 col0.x");
        assert_eq!(read_f32(64), 1280.0);
        assert_eq!(read_f32(68), 720.0);
        let read_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_u32(72), 77);
        assert_eq!(read_f32(76), 256.0);
        assert_eq!(read_f32(80), 0.25);
        assert!(bytes[84..].iter().all(|&b| b == 0));
    }
}
