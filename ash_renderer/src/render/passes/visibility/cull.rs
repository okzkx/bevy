//! GPU 视锥剔除的 compute 段：管线、push 排布、CPU 参照实现、诊断账面。可见标记环
//! 住在 [`crate::render::InstanceTables`]（instance_set b3）。
//!
//! - **视锥平面**：CPU 从 view_proj 提取六平面（[`frustum_planes`]），每帧随 push 进
//!   compute。clip volume 是 D3D 型（Z∈`[0,1]`），near = row2、far = row3−row2；
//!   reverse-Z 不改变半空间集合。
//! - **保守语义**：AABB 不被任何平面整盒排除即可见（p 顶点判定，法线无需归一化），
//!   宁多画不错杀。GPU 侧 8 角点世界推导与 CPU 参照同款，结果可逐位对比。
//! - **host 时序**：host 读写标记都在 `wait_for_slot` 之后，读数 = 本帧槽两帧前
//!   compute 的输出，计量允许陈旧 ≤2 帧。坑：同步验证层看不见 host 读写，fence 是
//!   这条链唯一的保证。
//! - **dispatch 上界** = 实例槽 bump bound（`used_object_slots`）：free list 归还的
//!   槽号恒小于 bump 游标，bound 覆盖全部在场/在途槽；bound 外的陈旧标记无人读。
//! - **主视图开关**：shadow 开启时 dispatch 不随剔除开关停（shadow 标记也在本 pass
//!   产出），所以启用位随 push 进 shader，关 = marks 恒写 1。坑：否则 dispatch 会
//!   覆盖 host 的全可见填充，剔除开关失效。

use ash::vk;
use bevy::math::{Mat4, Vec3, Vec4};
use bevy::prelude::Resource;

use crate::common::bytes::{put_f32s, put_u32};
use crate::common::error::Result;

use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::passes::shadow::shadow_map::SHADOW_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::Context;

pub(crate) static CULL_PIPELINE: PipelineDesc = PipelineDesc {
    label: "cull",
    shader: &shader_abi::CULL,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC, &SHADOW_SET_DESC],
    push: Some(PushRange { stages: vk::ShaderStageFlags::COMPUTE, size: CULL_PUSH_SIZE }),
    color_formats: &[],
    depth_format: None,
};

/// compute push 总长（pipeline layout 声明 + WGSL `PushCull` 同源）：
/// 6×vec4 平面（96B）+ bound（u32 @96）+ 主视图剔除启用位（u32 @100，0 = 关）+
/// 垫（到 16 对齐）= 112B。规范保证 maxPushConstantsSize ≥ 128，无需查询。
const CULL_PUSH_SIZE: u32 = 112;

/// [`pack_cull_push`] 的 WGSL 布局镜像（与 `cull.wgsl::PushCull` 对照）。
#[repr(C)]
struct CullPushLayout {
    planes: [[f32; 4]; 6],
    bound: u32,
    main_cull: u32,
    _pad: [u32; 2],
}

const _: () = {
    assert!(std::mem::offset_of!(CullPushLayout, bound) == 96);
    assert!(std::mem::offset_of!(CullPushLayout, main_cull) == 100);
    assert!(std::mem::size_of::<CullPushLayout>() as u32 == CULL_PUSH_SIZE);
};

/// 视锥六平面提取（Gribb-Hartmann，列主序 view_proj）：每个平面是 world 空间
/// 半空间 (a,b,c,d)，**dot(p, plane) ≥ 0 为内侧**。顺序 = 左/右/下/上/近/远；
/// 法线不归一化（内外判定只看符号）。
#[must_use]
pub fn frustum_planes(view_proj: &Mat4) -> [Vec4; 6] {
    let (r0, r1, r2, r3) = (view_proj.row(0), view_proj.row(1), view_proj.row(2), view_proj.row(3));
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
fn aabb_outside_plane(plane: Vec4, min: Vec3, max: Vec3) -> bool {
    let p = Vec3::new(
        if plane.x >= 0.0 { max.x } else { min.x },
        if plane.y >= 0.0 { max.y } else { min.y },
        if plane.z >= 0.0 { max.z } else { min.z },
    );
    plane.truncate().dot(p) + plane.w < 0.0
}

/// AABB 与视锥的保守可见判定（与 `cull.wgsl` 逐位同语义）。
#[must_use]
pub fn aabb_visible(planes: &[Vec4; 6], min: Vec3, max: Vec3) -> bool {
    !planes.iter().any(|&p| aabb_outside_plane(p, min, max))
}

/// 视锥平面 + dispatch 上界 + 主视图剔除启用位 → 112B compute push 字节（偏移按
/// [`CullPushLayout`] 静态断言表排布）。
#[must_use]
fn pack_cull_push(planes: &[Vec4; 6], bound: u32, main_cull: bool) -> [u8; CULL_PUSH_SIZE as usize] {
    let mut out = [0u8; CULL_PUSH_SIZE as usize];
    for (i, plane) in planes.iter().enumerate() {
        put_f32s(&mut out, i * 16, &plane.to_array());
    }
    put_u32(&mut out, 96, bound);
    put_u32(&mut out, 100, u32::from(main_cull));
    out
}

/// 剔除开关（egui checkbox 与 BRP `cull_set_enabled` 写，draw_frame 读）。
/// 关 = 主视图标记全可见：host 填充本帧槽，shadow 开启仍 dispatch 时 shader 按
/// push 启用位同写 1。
#[derive(Resource)]
pub struct CullSettings {
    pub enabled: bool,
}

impl Default for CullSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 剔除诊断读数（draw_frame 每帧刷新，egui 面板/BRP 读）。只做计量，不得用它决定
/// 画什么。读数 = 本帧槽两帧前 compute 的输出（host 直读，在 fence 之后）。
#[derive(Resource, Default)]
pub struct CullDiagnostics {
    /// 本帧剔除开关快照。
    pub enabled: bool,
    /// dispatch 上界（实例槽 bump bound）。
    pub bound: u32,
    /// 可见标记数（在场可绘行中 marks=1 的行数）。
    pub visible_count: u32,
    /// 参与统计的行数（= 本帧 DrawList 可绘行数）。
    pub rows_compared: usize,
    /// 读数所属帧槽（0/1，陈旧度 ≤2 帧的来源标注）。
    pub marks_frame: usize,
}

/// 一帧剔除 dispatch 的录制输入（draw_frame 组装，`record_frame` 消费）。
#[derive(Clone, Copy)]
pub struct CullDraw {
    pub pass: ComputeHandles,
    /// [`pack_cull_push`] 产物。
    pub push: [u8; CULL_PUSH_SIZE as usize],
    /// dispatch 组数 = bound 取整到 workgroup（64）。
    pub groups: u32,
}

/// 剔除 compute 管线（[instance_set, shadow set] 双集 + 112B push）。
#[derive(Resource)]
pub struct CullPass {
    pipeline: ComputePipeline,
}

impl CullPass {
    /// 建管线：shadow UBO（四 cascade 平面）与交错标记环在第二集（cull.wgsl group(1)）。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self { pipeline: ComputePipeline::new(ctx, &CULL_PIPELINE, layouts)? })
    }

    /// 组一帧 dispatch 输入。draw_frame 消费，录制走 [`record`]。
    #[must_use]
    pub fn draw(&self, planes: &[Vec4; 6], bound: u32, main_cull: bool) -> CullDraw {
        CullDraw {
            pass: self.pipeline.handles(),
            push: pack_cull_push(planes, bound, main_cull),
            groups: bound.div_ceil(64),
        }
    }
}

/// 录制一帧剔除 dispatch（`record_frame` 在渲染进场前调用）。交接屏障罩住
/// compute 写标记 → 后续 compute 读写：indirect 组装读 marks、写 gap_marks，遮挡
/// 回写 marks——同 buffer 的后续写要 WAW 依赖，dst access 含 SHADER_WRITE。dst stage
/// 是超集（含 VS）。host 读不靠它（屏障排不了 host 访问，靠 fence）。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    instance_set: DescSet,
    shadow_set: DescSet,
    cull: &CullDraw,
) {
    cull.pass.record(
        device,
        command_buffer,
        &[instance_set, shadow_set],
        &cull.push,
        cull.groups,
        Handoff::VertexComputeReadWrite,
    );
}

#[cfg(test)]
mod tests {
    use super::{aabb_outside_plane, aabb_visible, frustum_planes, pack_cull_push, CULL_PUSH_SIZE};
    use bevy::math::{Mat4, Vec3, Vec4};

    /// 恒等 view_proj 的六平面 = NDC 盒半空间（D3D 型 Z[0,1]），钉死行组合约定。
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

    /// 与 `conservative_world_aabb` 配合：非均匀缩放 + 旋转的世界盒在视锥内/外的
    /// 判定。投影用手构正交矩阵，只钉平面提取与求交语义，真实相机走 BRP 对照验。
    #[test]
    fn world_aabb_vs_frustum_matches_conservative_box() {
        use crate::render::conservative_world_aabb;
        // view = 平移 (0,0,-3)；正交半宽 2、view Z[-10,0] → clip Z[0,1]
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

    /// push 打包与布局断言一致（112B：平面 @0..96、bound @96、主视图启用位 @100）。
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
        let bytes = pack_cull_push(&planes, 77, true);
        assert_eq!(bytes.len(), CULL_PUSH_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(0), 1.0);
        assert_eq!(read_f32(92), 1.0, "第 6 平面 (0,0,-1,1) 的 w 在 92..96");
        let bound = u32::from_le_bytes(bytes[96..100].try_into().expect("4B"));
        assert_eq!(bound, 77);
        let main_cull = u32::from_le_bytes(bytes[100..104].try_into().expect("4B"));
        assert_eq!(main_cull, 1);
        assert_eq!(pack_cull_push(&planes, 77, false)[100..104], [0u8; 4], "关 = 0");
        // 104..112 垫保持 0
        assert!(bytes[104..].iter().all(|&b| b == 0));
    }
}
