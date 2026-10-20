//! GPU LOD 选档 compute 半边（6.2.3）：管线 + push 排布 + CPU 参照纯函数 +
//! 诊断账面。第三 compute pass——在遮挡测试之后、indirect 组装之前（档位
//! 环 b8 由本段写、build_indirect 读：命令三字段从 LOD 变体表按档直址，
//! instanceCount 语义不变）。
//!
//! 机制（对应 lod_select.wgsl 头注，此处只记 Rust 侧契约）：
//! - **判据**：屏幕覆盖尺寸（AABB 投屏 clamp 后最大边）——与 6.1 金字塔
//!   选层同族（一次投影两个消费端，段计划待决 1 倾向采纳）。自然档位 =
//!   floor(log2(threshold / s))，阈值/迟滞随 96B push 每帧进 compute。
//! - **迟滞**（判定线 2 口径）：向粗切换需越过自然边界 ×(1−h)，向细切换
//!   即时；prev 读档位环本槽 = 两帧前本帧槽输出（fence 纪律同标记环，
//!   2 帧滞后由迟滞带吸收，静态场景收敛值不受影响——6.2.3 钉明）。
//! - **fallback 无需专段**：选档每帧从当前相机现算，无跨帧状态依赖
//!   （档位环只是迟滞状态，不是真值）；未驻留档由 build_indirect 的
//!   表内回退承担（本段不做，见 indirect 模块）。
//! - **A/B 开关**（[`LodSettings`]）：关 = 不 dispatch，档位环 host 填 0
//!   （最细档）——命令三字段回到对象记录基准，画面语义 = 5.3 基线。
//! - **push**：view_proj（64B）+ screen（8B @64）+ bound（u32 @72）+
//!   threshold（f32 @76）+ hysteresis（f32 @80）+ 垫（到 96B 对齐 16），
//!   布局断言与 WGSL `PushLod` 静态互证（occlusion 80B 同款纪律；
//!   cull push 的 16B 垫余量在此兑现——96B ≤ maxPushConstantsSize 128B
//!   保底，无需查询）。
//! - **标记环纪律**：b8 读写都在 `wait_for_slot` fence 内（host 诊断读数
//!   = 本帧槽两帧前 compute 的输出）；dispatch 上界 = 实例槽 bump bound。

use ash::vk;
use bevy::math::Mat4;
use bevy::prelude::Resource;

use crate::common::error::Result;

use super::{Context, MAX_LOD_LEVELS};

/// shader 模块字节（build.rs 产物，spirv-val 已过）。
const LOD_SELECT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lod_select.spv"));

/// compute push 总长（layout 声明 + WGSL `PushLod` 同源）：
/// mat4（64B）+ screen（8B @64）+ bound（u32 @72）+ threshold（f32 @76）+
/// hysteresis（f32 @80）+ 垫（到 16 对齐）= 96B。
pub const LOD_PUSH_SIZE: u32 = 96;

/// [`pack_lod_push`] 的 WGSL 布局镜像（与 `lod_select.wgsl::PushLod` 静态互证）。
#[repr(C)]
#[allow(dead_code)]
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

/// view_proj + 屏幕尺寸 + dispatch 上界 + 判据参数 → 96B compute push 字节。
#[must_use]
pub fn pack_lod_push(
    view_proj: &Mat4,
    screen: [f32; 2],
    bound: u32,
    threshold_px: f32,
    hysteresis: f32,
) -> [u8; LOD_PUSH_SIZE as usize] {
    let mut out = [0u8; LOD_PUSH_SIZE as usize];
    for (i, col) in view_proj.to_cols_array_2d().iter().enumerate() {
        for (k, v) in col.iter().enumerate() {
            let o = i * 16 + k * 4;
            out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    out[64..68].copy_from_slice(&screen[0].to_le_bytes());
    out[68..72].copy_from_slice(&screen[1].to_le_bytes());
    out[72..76].copy_from_slice(&bound.to_le_bytes());
    out[76..80].copy_from_slice(&threshold_px.to_le_bytes());
    out[80..84].copy_from_slice(&hysteresis.to_le_bytes());
    out
}

// ===== CPU 参照纯函数（判定线 1/2 的库测试与 lod_census 对照本体）=====

/// 屏幕覆盖尺寸 → 自然档位：`floor(log2(threshold / s))`——s 每减半升一档，
/// clamp 进 `[0, max_levels-1]`（s ≥ 阈值 → 0 最细档；完全离屏 s=0 → 最粗档，
/// 反正不画）。与 lod_select.wgsl 的 raw 同式（floor 语义，半档不跳档）。
#[must_use]
pub fn lod_raw_level(size_px: f32, threshold_px: f32, max_levels: u32) -> u32 {
    let s = size_px.max(1.0);
    let raw = bevy::math::ops::log2(threshold_px.max(1.0) / s).floor();
    (raw as i64).clamp(0, i64::from(max_levels) - 1) as u32
}

/// 迟滞档位裁决：向粗（raw > prev）需 `s ≤ 该档自然边界 ×(1−h)`（越过阈值
/// 带才降档），向细（raw < prev）即时升档，相等保持——判定线 2 的状态机
/// 本体。prev 取档位环本槽值（两帧前输出，钉明见模块注释）。
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
/// 写，draw_frame 读）。关 = 不 dispatch 选档，档位环 host 填 0——数据层
/// A/B 的 B 面（判定线 5）；与 CullSettings/OcclusionSettings 分立（三开关
/// 独立成组，三开关矩阵的 A/B 归因）。
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

/// LOD 诊断账面（每帧 draw_frame 刷新，egui 面板/BRP 读数口；只进计量，
/// 任何系统不得读它决定画什么——5.2 同款边界）。档位读数 = 本帧槽两帧前
/// compute 的输出（HOST_VISIBLE 直读，fence 纪律内）。
#[derive(Resource, Default)]
pub struct LodDiagnostics {
    /// LOD 开关快照（与 [`LodSettings`] 同步）。
    pub enabled: bool,
    /// 本帧是否实际 dispatch（enabled ∧ 场内有实例槽）。
    pub dispatched: bool,
    /// dispatch 上界（实例槽 bump bound）。
    pub bound: u32,
    /// 档位分布（在场可绘行中各档行数——档位环直读，判定线 5 的读数口）。
    pub level_counts: [u32; MAX_LOD_LEVELS as usize],
    /// 参与统计的行数（= 本帧 DrawList 可绘行数）。
    pub rows_compared: usize,
    /// 读数所属帧槽。
    pub marks_frame: usize,
    /// 闸门空转首跑留痕旗标（首个 dispatch 帧收账一次）。
    pub first_run_logged: bool,
}

/// 一次选档 dispatch 的录制输入（draw_frame 组装，`record` 消费）。
#[derive(Clone, Copy)]
pub struct LodDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 96B push：view_proj + screen + bound + threshold + hysteresis。
    pub push: [u8; LOD_PUSH_SIZE as usize],
    /// dispatch 组数 = bound 取整到 workgroup（64）。
    pub groups: u32,
}

/// 选档 compute 管线 + pipeline layout（set2 单集 + 96B push）。
#[derive(bevy::prelude::Resource)]
pub struct LodPass {
    device: ash::Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl LodPass {
    /// 建管线链：shader module（build.rs 产物）→ layout（set2 借用 + 96B
    /// push）→ ComputePipeline（OcclusionPass 同款形状——本段只读 set2 的
    /// b0/b1、写 b8，无第二描述符集）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, set2_layout: vk::DescriptorSetLayout) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = LOD_SELECT_SPV
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
        }?;
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(LOD_PUSH_SIZE);
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(std::slice::from_ref(&set2_layout))
                        .push_constant_ranges(std::slice::from_ref(&push_range)),
                    None,
                )
                .inspect_err(|_| device.destroy_shader_module(module, None))?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"cs_main");
            let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
            match device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) {
                Ok(mut pipelines) => {
                    let pipeline = pipelines.pop().expect("单 info 必返单管线");
                    device.destroy_shader_module(module, None);
                    Ok(Self { device: ctx.device.clone(), layout, pipeline })
                }
                Err((pipelines, e)) => {
                    for p in pipelines {
                        device.destroy_pipeline(p, None);
                    }
                    device.destroy_pipeline_layout(layout, None);
                    device.destroy_shader_module(module, None);
                    Err(e.into())
                }
            }
        }
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
            pipeline: self.pipeline,
            layout: self.layout,
            push: pack_lod_push(view_proj, screen, bound, threshold_px, hysteresis),
            groups: bound.div_ceil(64),
        }
    }
}

/// 录制一帧选档 dispatch（遮挡测试之后、build_indirect 之前）：绑管线 →
/// set2 → push → dispatch → 屏障（档位环写 → build_indirect 读；与
/// occlusion::record 的合并标记收口同形状——compute 写 → 计算/图形读）。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    lod: &LodDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            lod.pipeline,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            lod.layout,
            0,
            std::slice::from_ref(&set2),
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            lod.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &lod.push,
        );
        device.cmd_dispatch(command_buffer, lod.groups, 1, 1);
        // 档位环写 → build_indirect 读（occlusion::record 的收口同形状：
        // dst 超集盖图形 VS 读与计算组装读两种消费形态）
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::VERTEX_SHADER | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&barrier),
            &[],
            &[],
        );
    }
}

impl Drop for LodPass {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成。先拆管线再拆 layout（set2 layout 是借用）。
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{lod_hysteresis_level, lod_raw_level, pack_lod_push, LOD_PUSH_SIZE};
    use super::MAX_LOD_LEVELS as MAX_LEVELS;
    use bevy::math::{Mat4, Vec3};

    /// 自然档位分带（判定线 1 的参照语义基准）：floor 语义下 s 每减半升一档，
    /// 边界值归更粗档（s = T/2 恰为档 1）。
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

    /// 迟滞状态机（判定线 2，最危险单点）：向粗需越过 (1−h) 边界、向细
    /// 即时、带内不抖动。
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
