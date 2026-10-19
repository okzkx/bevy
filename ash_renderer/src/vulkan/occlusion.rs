//! Hi-Z 遮挡测试 compute 半边（6.1.3）：管线 + push 排布 + CPU 参照投屏 +
//! 诊断账面。第二 compute pass——在视锥剔除之后、indirect 组装之前（标记的
//! 合并语义：遮挡把视锥标记清零，b3 = 视锥 ∧ 遮挡，build_indirect 消费形状
//! 不变；遮挡独立标记 b6 只进计量通道，判定线 4 的 A/B 归因读数口）。
//!
//! 机制（对应 occlusion.wgsl 头注，此处只记 Rust 侧契约）：
//! - **fallback 判据**（判定线 2）：相机跳变 / 金字塔 warmup 未就绪 / 开关
//!   关闭 / 视锥剔除关闭（A/B 面保持干净）→ 本帧整段跳过 dispatch，b3 保持
//!   视锥纯结果、b6 host 填全可见——跳过本身进诊断账（fallback 帧计数）。
//!   相机跳变阈值：位移 > 0.5 世界单位或旋转 > 15°/帧（钉明：连续缓慢环绕
//!   不触发——2 帧滞后的短暂误剔除由判定线 2"数帧内恢复"兜底，只对离散
//!   跳变显式让路）。
//! - **push**：view_proj（64B）+ screen（2×f32 像素）+ bound（u32）= 80B，
//!   布局断言与 WGSL `PushOcclusion` 静态互证（cull 112B 同款纪律）。
//! - **CPU 参照**：`aabb_screen_rect` 投屏（8 角点 → 像素矩形 + NDC z max，
//!   w ≤ 0 → None）——`occluded_reverse_z`/`occlusion_mip_level`（hiz 模块
//!   纯函数）拼出完整判定，供库测试与 6.1.5 的对照读数（不进绘制路径）。
//! - **标记环纪律**：b3/b6 读写都在 `wait_for_slot` fence 内（host 诊断读数
//!   = 本帧槽两帧前输出）；dispatch 上界 = 实例槽 bump bound（5.2 同款）。

use ash::vk;
use bevy::math::{Mat4, Vec2, Vec4};
use bevy::prelude::Resource;

use crate::common::error::Result;

use super::Context;

/// shader 模块字节（build.rs 产物，spirv-val 已过）。
const OCCLUSION_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/occlusion.spv"));

/// compute push 总长（layout 声明 + WGSL `PushOcclusion` 同源）：
/// mat4（64B）+ screen（8B @64）+ bound（u32 @72）+ 垫（到 16 对齐）= 80B。
pub const OCCLUSION_PUSH_SIZE: u32 = 80;

/// [`pack_occlusion_push`] 的 WGSL 布局镜像（与 `occlusion.wgsl::PushOcclusion`
/// 静态互证）。
#[repr(C)]
#[allow(dead_code)]
struct OcclusionPushLayout {
    view_proj: [[f32; 4]; 4],
    screen: [f32; 2],
    bound: u32,
    _pad: u32,
}

const _: () = {
    assert!(std::mem::offset_of!(OcclusionPushLayout, screen) == 64);
    assert!(std::mem::offset_of!(OcclusionPushLayout, bound) == 72);
    assert!(std::mem::size_of::<OcclusionPushLayout>() as u32 == OCCLUSION_PUSH_SIZE);
};

/// view_proj + 屏幕尺寸 + dispatch 上界 → 80B compute push 字节。
#[must_use]
pub fn pack_occlusion_push(view_proj: &Mat4, screen: [f32; 2], bound: u32) -> [u8; OCCLUSION_PUSH_SIZE as usize] {
    let mut out = [0u8; OCCLUSION_PUSH_SIZE as usize];
    for (i, col) in view_proj.to_cols_array_2d().iter().enumerate() {
        for (k, v) in col.iter().enumerate() {
            let o = i * 16 + k * 4;
            out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    out[64..68].copy_from_slice(&screen[0].to_le_bytes());
    out[68..72].copy_from_slice(&screen[1].to_le_bytes());
    out[72..76].copy_from_slice(&bound.to_le_bytes());
    out
}

/// AABB 投屏参照（CPU 侧，occlusion.wgsl 同语义）：8 世界角点全投影——
/// 像素矩形（min/max，Y 翻转与正高度视口直出同约定）+ NDC z 的 MAX（物体
/// 深度值上界）。任一角点 w ≤ 0 → None（恒判可见，与 shader 的早退同语义）。
#[must_use]
pub fn aabb_screen_rect(
    view_proj: &Mat4,
    model: &Mat4,
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
    screen: [f32; 2],
) -> Option<(Vec2, Vec2, f32)> {
    let mut px_min = Vec2::splat(f32::INFINITY);
    let mut px_max = Vec2::splat(f32::NEG_INFINITY);
    let mut depth_max = f32::NEG_INFINITY;
    for x in 0..2 {
        for y in 0..2 {
            for z in 0..2 {
                let corner = Vec4::new(
                    if x == 1 { aabb_max[0] } else { aabb_min[0] },
                    if y == 1 { aabb_max[1] } else { aabb_min[1] },
                    if z == 1 { aabb_max[2] } else { aabb_min[2] },
                    1.0,
                );
                let world = *model * corner;
                let clip = *view_proj * world;
                if clip.w <= 0.0 {
                    return None;
                }
                let ndc = Vec2::new(clip.x, clip.y) / clip.w;
                let px = (ndc * Vec2::new(0.5, -0.5) + Vec2::splat(0.5)) * Vec2::from_array(screen);
                px_min = px_min.min(px);
                px_max = px_max.max(px);
                depth_max = depth_max.max(clip.z / clip.w);
            }
        }
    }
    Some((px_min, px_max, depth_max))
}

/// 遮挡开关（egui checkbox 与 BRP `occlusion_set_enabled` 写，draw_frame 读）。
/// 关 = 不 dispatch 遮挡测试，b3 保持视锥纯结果、b6 host 填全可见——数据层
/// A/B 的 B 面（判定线 4）；与 5.2 的 `CullSettings` 分立（两个开关独立成组）。
#[derive(Resource)]
pub struct OcclusionSettings {
    pub enabled: bool,
}

impl Default for OcclusionSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 遮挡测试 fallback 原因（诊断读数口径；单帧可叠加，取首个命中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcclusionFallback {
    /// 开关关闭（用户 A/B）。
    Disabled,
    /// 视锥剔除关闭（A/B 面保持干净：遮挡不独立于视锥开启）。
    CullOff,
    /// 金字塔 warmup 未就绪（首建/resize 后前 2 帧）。
    PyramidWarmup,
    /// 相机跳变（平移 > 0.5 或旋转 > 15°，钉明见模块注释）。
    CameraJump,
}

/// 遮挡诊断账面（每帧 draw_frame 刷新，egui 面板/BRP 读数口；只进计量，
/// 任何系统不得读它决定画什么——5.2 同款边界）。读数 = 本帧槽两帧前 compute
/// 的输出（HOST_VISIBLE 直读，fence 纪律内）。
#[derive(Resource, Default)]
pub struct OcclusionDiagnostics {
    /// 遮挡开关快照（与 [`OcclusionSettings`] 同步）。
    pub enabled: bool,
    /// 本帧是否实际 dispatch（enabled ∧ 视锥开 ∧ 金字塔就绪 ∧ 相机稳定）。
    pub dispatched: bool,
    /// 未 dispatch 时的 fallback 原因（dispatched = true 时为 None）。
    pub fallback: Option<OcclusionFallback>,
    /// dispatch 上界（实例槽 bump bound）。
    pub bound: u32,
    /// 遮挡判可见数（occ=1 的在场行——视锥可见 ∧ 未被挡）。
    pub occlusion_visible_count: u32,
    /// 参与统计的行数（= 本帧 DrawList 可绘行数）。
    pub rows_compared: usize,
    /// 读数所属帧槽。
    pub marks_frame: usize,
    /// 闸门空转首跑留痕旗标（首个 dispatch 帧收账一次）。
    pub first_run_logged: bool,
}

/// 一帧遮挡 dispatch 的录制输入（draw_frame 组装，`record_frame` 消费）。
#[derive(Clone, Copy)]
pub struct OcclusionDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 80B push：view_proj + screen + bound（[`pack_occlusion_push`] 产物）。
    pub push: [u8; OCCLUSION_PUSH_SIZE as usize],
    /// dispatch 组数 = bound 取整到 workgroup（64）。
    pub groups: u32,
    /// 本帧槽的 Hi-Z 全链采样描述符（HizPyramid::occlusion_set）。
    pub hiz_set: vk::DescriptorSet,
}

/// 遮挡 compute 管线 + pipeline layout（[set2, hiz] 双集 + 80B push）。
#[derive(bevy::prelude::Resource)]
pub struct OcclusionPass {
    device: ash::Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl OcclusionPass {
    /// 建管线链：shader module（build.rs 产物）→ layout（set2 借用 + Hi-Z
    /// 独立小集 + 80B push）→ ComputePipeline（CullPass 同款形状）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, set2_layout: vk::DescriptorSetLayout, hiz_layout: vk::DescriptorSetLayout) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = OCCLUSION_SPV
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
            .size(OCCLUSION_PUSH_SIZE);
        let sets = [set2_layout, hiz_layout];
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&sets)
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
        hiz_set: vk::DescriptorSet,
    ) -> OcclusionDraw {
        OcclusionDraw {
            pipeline: self.pipeline,
            layout: self.layout,
            push: pack_occlusion_push(view_proj, screen, bound),
            groups: bound.div_ceil(64),
            hiz_set,
        }
    }
}

/// 录制一帧遮挡 dispatch（视锥剔除之后、build_indirect 之前）：绑管线 →
/// [set2, hiz] 双集 → push → dispatch → 屏障（遮挡写标记/标记合并 →
/// build_indirect 读；与 cull::record 的预置屏障同形状——compute 写 →
/// 图形/计算读）。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    occlusion: &OcclusionDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            occlusion.pipeline,
        );
        let sets = [set2, occlusion.hiz_set];
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            occlusion.layout,
            0,
            &sets,
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            occlusion.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &occlusion.push,
        );
        device.cmd_dispatch(command_buffer, occlusion.groups, 1, 1);
        // 合并标记写 → build_indirect 读（cull::record 的既有屏障只覆盖
        // 视锥段，本段的写需要自己的收口；dst 超集照抄——图形 VS 读与
        // 计算组装读两种消费形态）
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

impl Drop for OcclusionPass {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成（先拆管线再拆 layout；set2/hiz layout 都是
        // 借用，不在此拆）
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{aabb_screen_rect, pack_occlusion_push, OCCLUSION_PUSH_SIZE};
    use bevy::math::{Mat4, Vec3};

    /// push 打包与布局断言一致（80B：矩阵 @0..64、screen @64、bound @72）。
    #[test]
    fn pack_occlusion_push_layout() {
        let vp = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let bytes = pack_occlusion_push(&vp, [1280.0, 720.0], 77);
        assert_eq!(bytes.len(), OCCLUSION_PUSH_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(0), 1.0, "mat4 col0.x");
        assert_eq!(read_f32(56), 3.0, "mat4 col3.z = 平移 z");
        assert_eq!(read_f32(60), 1.0, "mat4 col3.w = 1");
        assert_eq!(read_f32(64), 1280.0);
        assert_eq!(read_f32(68), 720.0);
        let bound = u32::from_le_bytes(bytes[72..76].try_into().expect("4B"));
        assert_eq!(bound, 77);
        assert!(bytes[76..].iter().all(|&b| b == 0));
    }

    /// 投屏参照与视锥语义联动：正交相机（cull 单测同款矩阵）下盒 [-1,1]³
    /// 投满屏中央；平移出屏后矩形完全在屏外（clamp 前）。
    #[test]
    fn screen_rect_projection() {
        let view = Mat4::from_translation(Vec3::new(0.0, 0.0, -3.0));
        let ortho = Mat4::from_cols_array(&[
            0.5, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0, 1.0, 1.0,
        ]);
        let vp = ortho * view;
        let screen = [1280.0, 720.0];
        let (min, max, depth) =
            aabb_screen_rect(&vp, &Mat4::IDENTITY, [-1.0; 3], [1.0; 3], screen).expect("正交全在相机前");
        // 正交 x_c = 0.5x → NDC ±0.5 → 像素 x ∈ [320, 960]；y 同理翻转后 [180, 540]
        assert!((min.x - 320.0).abs() < 1.0 && (max.x - 960.0).abs() < 1.0);
        assert!((min.y - 180.0).abs() < 1.0 && (max.y - 540.0).abs() < 1.0);
        // 正交 z_c = 0.1·z_view + 1（col2 第 3 行 0.1/col3 第 3 行 1）：盒世界
        // z ∈ [-1,1] 经视图平移 z_view ∈ [-4,-2] → NDC z ∈ [0.6, 0.8] 的 MAX
        // = 0.8（最近角点）
        assert!((depth - 0.8).abs() < 1e-5);
        // 盒平移到相机背后：透视型 w = z_view（col2 的 w 分量 = 1、col3 = 0）
        // 下 w ≤ 0 → None（恒判可见，与 shader 早退同语义）
        let persp = Mat4::from_cols_array(&[
            0.5, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.1, 1.0, 0.0, 0.0, 0.0, 0.0,
        ]);
        let behind = Mat4::from_translation(Vec3::new(0.0, 0.0, -10.0));
        assert!(aabb_screen_rect(&(persp * view), &behind, [-1.0; 3], [1.0; 3], screen).is_none());
    }
}
