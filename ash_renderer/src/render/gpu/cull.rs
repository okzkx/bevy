//! GPU 视锥剔除（5.2）：compute 半边——管线 + push 排布 + CPU 参照实现 +
//! 诊断账面。可见标记 buffer（标记环）住在 [`InstanceTables`]（set2 b3，
//! 第四族帧槽环）；本模块只管"怎么算、怎么派、怎么验"。
//!
//! 机制（对应段施工计划 §2/§3）：
//!
//! - **视锥平面**：CPU 从 view_proj 提取六平面（Gribb-Hartmann 半空间，
//!   [`frustum_planes`]），随 push constants 进 compute——帧级全局量每帧一次
//!   push 是 push constants 的本职（5.1 退役的是逐对象 push；帧 UBO 128B 已满，
//!   不为 compute-only 数据扩图形侧 UBO）。D3D 型 clip volume（Z∈[0,1]），
//!   near = row2、far = row3−row2；reverse-Z 不改变半空间集合。
//! - **保守语义**：AABB 不被任何平面整盒排除即可见（p 顶点判定，法线无需
//!   归一化）——与 5.1 `conservative_world_aabb` 同一条"宁多画不错杀"链。
//!   GPU 侧 8 角点世界推导与 CPU 参照同款，判定线 1 逐位可比。
//! - **标记环**：HOST_VISIBLE storage ×`MAX_FRAMES_IN_FLIGHT`（instance_table
//!   第四族），本段消费者是宿主诊断读数；5.3 GPU 消费进来时评估迁
//!   DEVICE_LOCAL（翻面条件已入段施工记录）。
//! - **时序**：host 读/写标记与环写入同一条 fence 证据——都在 `wait_for_slot`
//!   之后（本帧槽上轮 fence 已过，读数 = 两帧前 compute 的输出，计量允许陈旧
//!   ≤2 帧）。**同步验证层对 host 写/读全盲**，fence 是此链唯一执法（与
//!   模型环同一纪律，见 transient 环笔记）。
//! - **dispatch 上界** = 实例槽 bump bound（`used_object_slots`）：free list
//!   归还槽号恒小于 bump 游标，bound 覆盖全部在场/在途槽；bound 外的陈旧
//!   标记无人读（账本行只枚举在场槽）。
//! - **屏障**：dispatch 后立一条 compute 写→图形/计算读的全局内存屏障——
//!   本段图形侧尚无标记读者（消费归 5.3），屏障是 5.3 依赖形状的预置与合法性
//!   验证（判定线 3）；宿主读不靠它（barrier 排不了 host 访问，靠 fence）。

use ash::vk;
use bevy::math::{Mat4, Vec3, Vec4};
use bevy::prelude::Resource;

use crate::common::error::Result;

use crate::render::Context;

/// shader 模块字节（build.rs 产物，OUT_DIR 之下；compute 编译链与图形同一闸门）。
const CULL_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cull.spv"));

/// compute push 总长（pipeline layout 声明 + WGSL `PushCull` 同源）：
/// 6×vec4 平面（96B）+ bound（u32 @96）+ 垫（到 16 对齐）= 112B。
/// 规范保证 maxPushConstantsSize ≥ 128，112B 在一切实现上合法（无需查询）。
pub const CULL_PUSH_SIZE: u32 = 112;

/// [`pack_cull_push`] 的 WGSL 布局镜像（与 `cull.wgsl::PushCull` 静态互证）。
#[repr(C)]
#[allow(dead_code)]
struct CullPushLayout {
    planes: [[f32; 4]; 6],
    bound: u32,
    _pad: [u32; 3],
}

const _: () = {
    assert!(std::mem::offset_of!(CullPushLayout, bound) == 96);
    assert!(std::mem::size_of::<CullPushLayout>() as u32 == CULL_PUSH_SIZE);
};

/// 视锥六平面提取（Gribb-Hartmann，列主序 view_proj）：每个平面是 world 空间
/// 半空间 (a,b,c,d)，**dot(p, plane) ≥ 0 为内侧**。顺序 = 左/右/下/上/近/远；
/// 法线不归一化（纯内外判定只看符号与相对零点，归一化只在要"距离"时有意义）。
#[must_use]
pub fn frustum_planes(view_proj: &Mat4) -> [Vec4; 6] {
    // 列主序展平：a[i*4+j] = 第 i 列第 j 行；数学第 j 行 = 各列第 j 分量
    //（to_cols_array 同 pack_frame_uniforms 的展平约定）。
    let a = view_proj.to_cols_array();
    let row = |j: usize| Vec4::new(a[j], a[4 + j], a[8 + j], a[12 + j]);
    let (r0, r1, r2, r3) = (row(0), row(1), row(2), row(3));
    [
        r0 + r3,        // x_c + w_c ≥ 0（左）
        r3 - r0,        // w_c − x_c ≥ 0（右）
        r1 + r3,        // y_c + w_c ≥ 0（下）
        r3 - r1,        // w_c − y_c ≥ 0（上）
        r2,             // z_c ≥ 0（近，D3D 型 clip volume）
        r3 - r2,        // w_c − z_c ≥ 0（远）
    ]
}

/// AABB 对单平面的整盒排除判定：法线同号侧的 p 顶点落在半空间外 = 全盒在外。
#[must_use]
pub fn aabb_outside_plane(plane: Vec4, min: Vec3, max: Vec3) -> bool {
    let p = Vec3::new(
        if plane.x >= 0.0 { max.x } else { min.x },
        if plane.y >= 0.0 { max.y } else { min.y },
        if plane.z >= 0.0 { max.z } else { min.z },
    );
    Vec3::new(plane.x, plane.y, plane.z).dot(p) + plane.w < 0.0
}

/// AABB 与视锥的保守可见判定（与 `cull.wgsl` 逐位同语义）：不被任何平面
/// 整盒排除即可见——相交即可见，宁多画不错杀。
#[must_use]
pub fn aabb_visible(planes: &[Vec4; 6], min: Vec3, max: Vec3) -> bool {
    !planes.iter().any(|&p| aabb_outside_plane(p, min, max))
}

/// 视锥平面 + dispatch 上界 → 112B compute push 字节（偏移按 [`CullPushLayout`]
/// 静态断言表手工排布，与 pack_frame_uniforms 同一纪律）。
#[must_use]
pub fn pack_cull_push(planes: &[Vec4; 6], bound: u32) -> [u8; CULL_PUSH_SIZE as usize] {
    let mut out = [0u8; CULL_PUSH_SIZE as usize];
    for (i, plane) in planes.iter().enumerate() {
        for (k, v) in plane.to_array().iter().enumerate() {
            let o = i * 16 + k * 4;
            out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    out[96..100].copy_from_slice(&bound.to_le_bytes());
    out
}

/// 剔除开关（egui checkbox 与 BRP `cull_set_enabled` 写，draw_frame 读）。
/// 关 = 不 dispatch，host 把本帧槽标记填全可见（数据层 A/B 的 B 面：标记
/// 语义基准）；画面两条路径都不消费，画面恒与 5.1 基线一致。
#[derive(Resource)]
pub struct CullSettings {
    pub enabled: bool,
}

impl Default for CullSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 剔除诊断账面（每帧 draw_frame 刷新，egui 面板/BRP 读数口；只进计量，
/// 任何系统不得读它决定画什么——判定线 2 的"诊断性回读"边界）。
/// 读数 = 本帧槽两帧前 compute 的输出（HOST_VISIBLE 直读，fence 纪律内）。
#[derive(Resource, Default)]
pub struct CullDiagnostics {
    /// 本帧剔除开关（与 [`CullSettings`] 同步，读数快照口径）。
    pub enabled: bool,
    /// dispatch 上界（实例槽 bump bound）。
    pub bound: u32,
    /// 可见标记数（在场可绘行中 marks=1 的行数）。
    pub visible_count: u32,
    /// 参与统计的行数（= 本帧 DrawList 可绘行数）。
    pub rows_compared: usize,
    /// 读数所属帧槽（0/1，陈旧度 ≤2 帧的来源标注）。
    pub marks_frame: usize,
    /// 闸门④空转首跑（首个 dispatch 帧）留痕旗标。
    pub first_dispatch_logged: bool,
}

/// 一帧剔除 dispatch 的录制输入（draw_frame 组装，`record_frame` 消费）。
#[derive(Clone, Copy)]
pub struct CullDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 112B push：六平面 + bound（[`pack_cull_push`] 产物）。
    pub push: [u8; CULL_PUSH_SIZE as usize],
    /// dispatch 组数 = bound 取整到 workgroup（64）。
    pub groups: u32,
}

/// compute 管线 + pipeline layout（set2 单集 + 112B push）。
#[derive(bevy::prelude::Resource)]
pub struct CullPass {
    device: ash::Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl CullPass {
    /// 建管线链：shader module（build.rs 产物，spirv-val 已过）→ layout
    ///（只借 set2 + COMPUTE push range）→ ComputePipeline（无附件/状态纠缠，
    /// 比图形管线简单——前置闸门④的载体）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        set2_layout: vk::DescriptorSetLayout,
        shadow_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = CULL_SPV
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
            .size(CULL_PUSH_SIZE);
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    // 8.3：[set2, shadow set] 双集——shadow UBO（四 cascade
                    // 平面）与交错标记环在第二集（cull.wgsl group(1)）
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[set2_layout, shadow_layout])
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
                    // 半建失败：已返回的句柄也要拆（ash 合约）
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

    /// 组一帧 dispatch 输入：平面 + bound 打包进 push，组数 = bound 取整到
    /// workgroup（64）。draw_frame 消费，录制走 [`record`]。
    #[must_use]
    pub fn draw(&self, planes: &[Vec4; 6], bound: u32) -> CullDraw {
        CullDraw {
            pipeline: self.pipeline,
            layout: self.layout,
            push: pack_cull_push(planes, bound),
            groups: bound.div_ceil(64),
        }
    }
}

/// 录制一帧剔除 dispatch（`record_frame` 在渲染进场前调用）：绑 compute 管线
/// → set2 → push（平面+bound）→ dispatch → 5.3 预置屏障（compute 写 → 图形/
/// 计算读，本段无真实读者，合法性由验证层执法——判定线 3）。输入全在
/// [`CullDraw`]（pipeline/layout/push/groups），录制层不需要 CullPass 句柄。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    shadow_set: vk::DescriptorSet,
    cull: &CullDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, cull.pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            cull.layout,
            0,
            &[set2, shadow_set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            cull.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &cull.push,
        );
        device.cmd_dispatch(command_buffer, cull.groups, 1, 1);
        // 5.3 消费预置：compute 写标记 → 后续图形（VS 读）或计算（indirect
        // 组装）读。本段无真实读者，屏障零成本；dst stage 超集盖两种消费形态。
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

impl Drop for CullPass {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成。先拆管线再拆 layout（借用 set2 layout 归
        // InstanceTables，不在此拆）。
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{aabb_outside_plane, aabb_visible, frustum_planes, pack_cull_push, CULL_PUSH_SIZE};
    use bevy::math::{Mat4, Vec3, Vec4};

    /// 恒等 view_proj 的六平面 = NDC 盒半空间（D3D 型 Z[0,1]）——提取的
    /// 行组合约定被本测试钉死（判定线 1 的参照语义基准）。
    #[test]
    fn identity_planes_are_ndc_box() {
        let planes = frustum_planes(&Mat4::IDENTITY);
        let close = |a: Vec4, b: [f32; 4]| a.to_array().iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(close(planes[0], [1.0, 0.0, 0.0, 1.0]), "左 = row0+row3");
        assert!(close(planes[1], [-1.0, 0.0, 0.0, 1.0]), "右 = row3−row0");
        assert!(close(planes[2], [0.0, 1.0, 0.0, 1.0]), "下 = row1+row3");
        assert!(close(planes[3], [0.0, -1.0, 0.0, 1.0]), "上 = row3−row1");
        assert!(close(planes[4], [0.0, 0.0, 1.0, 0.0]), "近 = row2（Z≥0）");
        assert!(close(planes[5], [0.0, 0.0, -1.0, 1.0]), "远 = row3−row2");
    }

    /// 恒等视锥（= 盒 [-1,1]²×[0,1]）下的可见性判定：内部/跨界/外部三态。
    #[test]
    fn identity_frustum_visibility() {
        let planes = frustum_planes(&Mat4::IDENTITY);
        // 盒内
        assert!(aabb_visible(&planes, Vec3::splat(-0.5), Vec3::splat(0.5)));
        // 跨左边界（保守 = 可见）
        assert!(aabb_visible(&planes, Vec3::new(-1.5, -0.5, 0.5), Vec3::new(-0.5, 0.5, 0.5)));
        // 整盒在左侧外（x < -1）
        assert!(!aabb_visible(&planes, Vec3::new(-3.0, -0.5, 0.5), Vec3::new(-2.0, 0.5, 0.5)));
        // 整盒在近平面外（z < 0，D3D 型近半空间 z ≥ 0）
        assert!(!aabb_visible(&planes, Vec3::new(-0.5, -0.5, -2.0), Vec3::new(0.5, 0.5, -0.1)));
        // 跨近平面（保守 = 可见）
        assert!(aabb_visible(&planes, Vec3::new(-0.5, -0.5, -0.5), Vec3::new(0.5, 0.5, 0.5)));
        // 整盒在远处外（z > 1）
        assert!(!aabb_visible(&planes, Vec3::new(-0.5, -0.5, 2.0), Vec3::new(0.5, 0.5, 3.0)));
    }

    /// 单平面 p 顶点选择的方向性：法线分量符号决定取 max/min 侧。
    #[test]
    fn p_vertex_selection() {
        let plane = Vec4::new(1.0, -1.0, 0.0, 0.0);
        // p 顶点 = (+max, +min)；盒 [0,1]³ 的 p 顶点 (1,0,·) 距离 = 1 ≥ 0 在内
        assert!(!aabb_outside_plane(plane, Vec3::ZERO, Vec3::ONE));
        // 盒整体在半空间外：x ≤ -2、y ≥ 2 区域
        assert!(aabb_outside_plane(
            plane,
            Vec3::new(-4.0, 2.0, 0.0),
            Vec3::new(-2.0, 4.0, 1.0),
        ));
    }

    /// 与 5.1 `conservative_world_aabb` 的配合：非均匀缩放 + 旋转的世界盒
    /// 在视锥内/外的判定（判定线 1 的端到端形状）。投影用正交（bevy
    /// Projection 的 directx 变体取用要过组件，单元测试只钉平面提取与求交
    /// 语义——真实相机的 view_proj 走 BRP 对照验）。
    #[test]
    fn world_aabb_vs_frustum_matches_conservative_box() {
        use crate::render::conservative_world_aabb;
        // 相机在原点看 -Z：view = 平移 (0,0,-3)；正交半宽 2、view Z[-10,0] →
        // clip Z[0,1]（D3D 型深度，与帧 UBO 同一 clip volume 约定——矩阵手构
        // 避免 glam 投影 API 的型别往返，行组合在 identity 测试里已钉）
        let view = Mat4::from_translation(Vec3::new(0.0, 0.0, -3.0));
        let ortho = Mat4::from_cols_array(&[
            0.5, 0.0, 0.0, 0.0, // col0：x_c = 0.5x
            0.0, 0.5, 0.0, 0.0, // col1：y_c = 0.5y
            0.0, 0.0, 0.1, 0.0, // col2：z_c = 0.1z
            0.0, 0.0, 1.0, 1.0, // col3：z_c += 1，w_c = 1
        ]);
        let planes = frustum_planes(&(ortho * view));
        let model = Mat4::from_rotation_y(std::f32::consts::FRAC_PI_6)
            * Mat4::from_scale(Vec3::new(2.0, 0.5, 3.0));
        let (wmin, wmax) = conservative_world_aabb(&model, [-1.0; 3], [1.0; 3]);
        // 世界盒中心贴原点：跨左右边界 → 保守可见
        assert!(aabb_visible(
            &planes,
            Vec3::from_array(wmin),
            Vec3::from_array(wmax),
        ));
        // 平移到 x=+20 远外侧：整盒必被排除
        let shift = Vec3::new(20.0, 0.0, 0.0);
        assert!(!aabb_visible(
            &planes,
            Vec3::from_array(wmin) + shift,
            Vec3::from_array(wmax) + shift,
        ));
    }

    /// push 打包与布局断言一致（112B：平面 @0..96、bound @96）。
    #[test]
    fn pack_cull_push_layout() {
        let planes = [
            Vec4::new(1.0, 0.0, 0.0, 1.0),
            Vec4::new(-1.0, 0.0, 0.0, 1.0),
            Vec4::new(0.0, 1.0, 0.0, 1.0),
            Vec4::new(0.0, -1.0, 0.0, 1.0),
            Vec4::new(0.0, 0.0, 1.0, 0.0),
            Vec4::new(0.0, 0.0, -1.0, 1.0),
        ];
        let bytes = pack_cull_push(&planes, 77);
        assert_eq!(bytes.len(), CULL_PUSH_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(0), 1.0);
        assert_eq!(read_f32(92), 1.0, "第 6 平面 (0,0,-1,1) 的 w 在 92..96");
        let bound = u32::from_le_bytes(bytes[96..100].try_into().expect("4B"));
        assert_eq!(bound, 77);
        // 100..112 垫保持 0
        assert!(bytes[100..].iter().all(|&b| b == 0));
    }
}
