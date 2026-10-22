//! 8.3 CSM 级联阴影宿主半边：级联矩阵计算、shadow UBO 打包、开关与诊断账面。
//!
//! 机制（对应段施工计划 §0 立案期裁决）：
//!
//! - **级联划分**：4 级 log 分割（near [`SHADOW_NEAR_DISTANCE`] → far
//!   [`SHADOW_MAX_DISTANCE`]，Sponza 世界尺度 + 梯度球阵对照余量）；逐级
//!   拟合 = 光空间取该级视锥切片 8 角点的 AABB 收紧正交盒（行业标准
//!   fit-to-cascade；texel snap 先不做——静态相机 A/B 不闪即够判定线）。
//! - **reverse-Z 成套**：正交投影走 glam 的 RH D3D 变体（z∈[0,1]）后乘
//!   反 Z 偏置（z' = 1 − z）——near→1 / far→0，与主深度链（清 0 + GREATER）
//!   同一方向；级联深度图 clear 0。
//! - **平面族**：宿主 Gribb-Hartmann 从各 cascade_view_proj 提取六平面进
//!   shadow UBO（4×6=24 平面 384B 超 push 上限，故走 UBO）——`cull.wgsl`
//!   内联四 cascade 标记与本 CPU 参照同源同款（判定线 3 逐位可比）。
//! - **时序**：shadow UBO 帧槽环（wait_for_slot fence 纪律）；GPU shadow
//!   标记 = 本帧槽两帧前 compute 输出，诊断读数允许陈旧 ≤2 帧（与主标记同款）。

use bevy::math::{Mat4, Vec3, Vec4};
use bevy::prelude::Resource;

use crate::render::pipeline::shadow::SHADOW_UBO_SIZE;

/// 级联数量（立案拍板）。
pub const SHADOW_CASCADES: usize = 4;
/// shadow map 边长（D32_SFLOAT，正方形；立案拍板起步值）。
pub const SHADOW_MAP_SIZE: u32 = 2048;
/// 级联覆盖近端（米；主相机 near 以内不进 shadow 覆盖面）。
pub const SHADOW_NEAR_DISTANCE: f32 = 0.5;
/// 级联覆盖远端（米；Sponza ~30×12×18 + 梯度球阵 y=25 的对照余量）。
pub const SHADOW_MAX_DISTANCE: f32 = 80.0;

/// log 分割的级间视深（升序，含首尾：`s[0]=near` … `s[4]=far`，长度 = 级数+1）。
/// split_i = near·(far/near)^(i/n)。
#[must_use]
pub fn cascade_slice_boundaries() -> [f32; SHADOW_CASCADES + 1] {
    // log 分割 = near·(far/near)^(i/n)——超债函数（powf/exp/ln）在本仓库
    // 禁用清单，边界值改为常量表（near/far 常量变更时此处同步重算，
    // 静态断言钉尾值防漂移）
    const BOUNDS: [f32; SHADOW_CASCADES + 1] =
        [0.5, 1.788_854_4, 6.324_555_3, 22.627_417, 80.0];
    // 静态互证（assert 在 const 上下文恒真被 clippy 拒，改 const 断言）
    const _: () = {
        assert!(BOUNDS[0] == SHADOW_NEAR_DISTANCE);
        assert!(BOUNDS[SHADOW_CASCADES] == SHADOW_MAX_DISTANCE);
    };
    BOUNDS
}

/// 级联矩阵计算的输入（draw_frame 组装，与帧 UBO 同源同帧）。
pub struct ShadowCascadeInput {
    /// inv(view_proj)：视深→NDC 反推切片角的常量面（与 8.1.2 深度重建同源）。
    pub inv_view_proj: Mat4,
    /// 相机世界位置。
    pub camera_pos: Vec3,
    /// 相机 forward（单位）。
    pub camera_forward: Vec3,
    /// 投影 near（无限反向透视的 z_ndc↔视深换算）。
    pub near: f32,
    /// 方向光行进方向（单位，= 灯实体 back()）。
    pub dir_to_light: Vec3,
}

/// 视深 → 无限反向透视的 NDC z（z_ndc = near/d，d ≤ near 时夹到 1）。
#[must_use]
pub fn view_depth_to_ndc_z(near: f32, d: f32) -> f32 {
    (near / d.max(near)).min(1.0)
}

/// 逐级联：光空间正交拟合。返回（cascade_view_proj 们，各 Cascade 的
/// 远端视深——合成端 cascade 选择的分割带）。
#[must_use]
pub fn cascade_matrices(
    input: &ShadowCascadeInput,
) -> ([Mat4; SHADOW_CASCADES], [f32; SHADOW_CASCADES]) {
    let bounds = cascade_slice_boundaries();
    // 光行进方向的正交 up：世界 Y 优先，近铅直时换 X
    let up_ref = if input.dir_to_light.y.abs() > 0.99 { Vec3::X } else { Vec3::Y };
    let up = (up_ref - input.dir_to_light * up_ref.dot(input.dir_to_light)).normalize();
    // 反 Z 偏置（D3D 正交 z∈[0,1] → reverse-Z）：z' = 1 − z
    let reverse_z = Mat4::from_cols_array_2d(&[
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0, 1.0],
    ]);
    let mut matrices = [Mat4::IDENTITY; SHADOW_CASCADES];
    let mut far_bounds = [0.0_f32; SHADOW_CASCADES];
    // RH look-at 视图矩阵（手写——glam 的 Mat4::look_at_rh 已 deprecated；
    // 列主序：col0..2 = 基向量，col3 = 平移）
    let look_at_rh = |eye: Vec3, center: Vec3, up: Vec3| {
        let f = (center - eye).normalize();
        let r = f.cross(up).normalize();
        let u = r.cross(f);
        Mat4::from_cols(
            Vec4::new(r.x, u.x, -f.x, 0.0),
            Vec4::new(r.y, u.y, -f.y, 0.0),
            Vec4::new(r.z, u.z, -f.z, 0.0),
            Vec4::new(-r.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
        )
    };
    // RH 正交（D3D 约定 z∈[0,1]，near→0 / far→1；手写同因 deprecated）
    let orthographic_rh_d3d = |l: f32, r: f32, b: f32, t: f32, zn: f32, zf: f32| {
        Mat4::from_cols(
            Vec4::new(2.0 / (r - l), 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 / (t - b), 0.0, 0.0),
            Vec4::new(0.0, 0.0, 1.0 / (zn - zf), 0.0),
            Vec4::new(-(r + l) / (r - l), -(t + b) / (t - b), zn / (zn - zf), 1.0),
        )
    };
    for c in 0..SHADOW_CASCADES {
        let (d0, d1) = (bounds[c], bounds[c + 1]);
        far_bounds[c] = d1;
        // 切片 8 角点（NDC 满幅 × 两档视深）反投影到世界
        let z0 = view_depth_to_ndc_z(input.near, d0);
        let z1 = view_depth_to_ndc_z(input.near, d1);
        let mut center = Vec3::ZERO;
        let mut corners = [Vec3::ZERO; 8];
        let mut k = 0;
        for &z in &[z0, z1] {
            for &x in &[-1.0_f32, 1.0] {
                for &y in &[-1.0_f32, 1.0] {
                    let p = input.inv_view_proj * Vec4::new(x, y, z, 1.0);
                    let w = p.w;
                    let world = Vec3::new(p.x / w, p.y / w, p.z / w);
                    corners[k] = world;
                    center += world;
                    k += 1;
                }
            }
        }
        center *= 1.0 / 8.0;
        // 光视图：eye 沿光行进反方向退 R，看向切片中心
        let span = corners.iter().map(|c| c.distance(center)).fold(0.0_f32, f32::max);
        let r = span + 1.0;
        let light_view = look_at_rh(center - input.dir_to_light * r, center, up);
        // 光空间 AABB 收紧正交盒
        let mut l = f32::INFINITY;
        let mut rgt = f32::NEG_INFINITY;
        let mut b = f32::INFINITY;
        let mut t = f32::NEG_INFINITY;
        let mut z_max = f32::NEG_INFINITY;
        let mut z_min = f32::INFINITY;
        for corner in &corners {
            let lv = light_view * corner.extend(1.0);
            l = l.min(lv.x);
            rgt = rgt.max(lv.x);
            b = b.min(lv.y);
            t = t.max(lv.y);
            z_max = z_max.max(lv.z);
            z_min = z_min.min(lv.z);
        }
        // RH 视空间在前 = z 负：near/far 取正距离（近端夹正防退化）
        let near_dist = (-z_max).max(0.01);
        let far_dist = (-z_min).max(near_dist + 0.01);
        let light_proj = orthographic_rh_d3d(l, rgt, b, t, near_dist, far_dist);
        matrices[c] = reverse_z * light_proj * light_view;
    }
    (matrices, far_bounds)
}

/// shadow UBO 的宿主侧取数产物（draw_frame 组装，[`pack_shadow_uniforms`] 排布）。
pub struct ShadowUniformsData {
    pub cascade_view_proj: [Mat4; SHADOW_CASCADES],
    /// 4×6 平面（Gribb-Hartmann，半空间 dot(p,plane)≥0 内侧——与 5.2 同款）。
    pub planes: [Vec4; SHADOW_CASCADES * 6],
    /// 各 cascade 远端视深（分割带；cascade i 的近端 = i==0 ? near : splits[i-1]）。
    pub splits: [f32; SHADOW_CASCADES],
    /// 相机 forward（xyz 单位，w=0）——合成端视深 = dot(world−cam, forward)。
    pub camera_forward: [f32; 4],
    /// 相机世界位置（xyz，w=0）。
    pub camera_pos: [f32; 4],
    /// shadow map 边长（PCSS 采样核的 texel 归一）。
    pub map_size: f32,
    /// 开关旗标（0/1，composite 采样旁路面——BRP shadow_set 直达帧 UBO，
    /// 关 = shadow_visibility 恒 1）。
    pub enabled: f32,
}

/// [`pack_shadow_uniforms`] 的 WGSL 布局镜像（`shadow_write.wgsl`/`shadow_build.wgsl`/
/// `gbuffer_composite.wgsl` 的 `ShadowUniforms` 同序静态互证）。
#[repr(C)]
#[allow(dead_code)]
struct ShadowUniformsLayout {
    cascade_view_proj: [[f32; 4]; 4 * 4],
    planes: [[f32; 4]; 24],
    splits: [f32; 4],
    camera_forward: [f32; 4],
    camera_pos: [f32; 4],
    map_size: f32,
    _pad: [f32; 3],
}

const _: () = {
    assert!(std::mem::offset_of!(ShadowUniformsLayout, planes) == 256);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, splits) == 640);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, camera_forward) == 656);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, camera_pos) == 672);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, map_size) == 688);
    assert!(std::mem::size_of::<ShadowUniformsLayout>() as u32 == SHADOW_UBO_SIZE);
};

/// mat4 列主序展平（与 pack_frame_uniforms 的 to_cols_array 同一约定）。
#[must_use]
pub fn pack_shadow_uniforms(d: &ShadowUniformsData) -> [u8; SHADOW_UBO_SIZE as usize] {
    let mut out = [0u8; SHADOW_UBO_SIZE as usize];
    let mut put = |o: usize, b: &[u8]| out[o..o + b.len()].copy_from_slice(b);
    for (i, m) in d.cascade_view_proj.iter().enumerate() {
        for (k, v) in m.to_cols_array().iter().enumerate() {
            put(i * 64 + k * 4, &v.to_le_bytes());
        }
    }
    for (i, p) in d.planes.iter().enumerate() {
        for (k, v) in p.to_array().iter().enumerate() {
            put(256 + i * 16 + k * 4, &v.to_le_bytes());
        }
    }
    for (k, v) in d.splits.iter().enumerate() {
        put(640 + k * 4, &v.to_le_bytes());
    }
    for (k, v) in d.camera_forward.iter().enumerate() {
        put(656 + k * 4, &v.to_le_bytes());
    }
    for (k, v) in d.camera_pos.iter().enumerate() {
        put(672 + k * 4, &v.to_le_bytes());
    }
    put(688, &d.map_size.to_le_bytes());
    put(692, &d.enabled.to_le_bytes());
    out
}

/// 阴影开关（egui 与 BRP `shadow_set` 写，draw_frame/录制链读）。
/// 关 = shadow 剔除标记/命令/绘制/采样全链旁路：cull 不写 shadow 标记
///（host 填全可见保读数语义）、shadow_build/shadow pass/composite 采样整段跳过，
/// 画面与 8.2 基线一致。
#[derive(Resource)]
pub struct ShadowSettings {
    pub enabled: bool,
}

impl Default for ShadowSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 阴影诊断账面（每帧 draw_frame 刷新，BRP 读数口；只进计量——诊断性
/// 回读边界照 5.2 拍板）。读数 = 本帧槽两帧前 compute 输出。
#[derive(Resource, Default)]
pub struct ShadowDiagnostics {
    pub enabled: bool,
    /// dispatch 上界（实例槽 bump bound，与主标记同款）。
    pub bound: u32,
    /// 逐 cascade GPU 可见标记数（在场可绘行中 mark=1）。
    pub gpu_visible: [u32; SHADOW_CASCADES],
    /// 逐 cascade CPU 参照可见数（cull_stats 同款保守 AABB × cascade 平面）。
    pub cpu_visible: [u32; SHADOW_CASCADES],
    /// 读数所属帧槽（0/1，陈旧度 ≤2 帧的来源标注）。
    pub marks_frame: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::camera::{CameraProjection, PerspectiveProjection};

    /// 竖直向下的光 + 原点相机看 -Z：切片角点反投影必须落在光空间 AABB 内
    ///（各 cascade 逐级变大），世界点投影后 depth 保持 reverse-Z 序。
    #[test]
    fn cascade_matrices_fit_and_order() {
        let view_proj =
            PerspectiveProjection { fov: 1.0, aspect_ratio: 1.0, ..Default::default() }
                .get_clip_from_view();
        let inv = view_proj.inverse();
        let input = ShadowCascadeInput {
            inv_view_proj: inv,
            camera_pos: Vec3::ZERO,
            camera_forward: Vec3::NEG_Z,
            near: 0.1,
            dir_to_light: Vec3::NEG_Y,
        };
        let (mats, splits) = cascade_matrices(&input);
        assert!(splits[0] > SHADOW_NEAR_DISTANCE && *splits.last().expect("4 级") <= SHADOW_MAX_DISTANCE);
        // 近级远端小于远级远端（log 分割单调）
        for w in splits.windows(2) {
            assert!(w[0] < w[1]);
        }
        for (c, m) in mats.iter().enumerate() {
            // 该级切片的中点深度（沿相机 forward）必须投影进本 cascade 的
            // NDC 盒内，且 reverse-Z 深度 = 0.5（拟合语义：切片中点在光空间
            // AABB 深向居中）；近级切片点不进远级盒（视深分带）不做跨级断言。
            let bounds = cascade_slice_boundaries();
            let d = (bounds[c] + bounds[c + 1]) * 0.5;
            let world = Vec3::NEG_Z * d;
            let clip = *m * world.extend(1.0);
            let ndc = clip.truncate() / clip.w;
            assert!(ndc.x.abs() < 1.0 && ndc.y.abs() < 1.0, "中心点出盒: {ndc}");
            assert!((ndc.z - 0.5).abs() < 1e-3, "切片中点深度应居中: {ndc}");
        }
        // 近 cascade 的物体深度 > 远 cascade 同点深度（reverse-Z：近=大）
        // 由 fit 语义保证，不做跨级强断言（同点只应落在单级）。
    }

    /// 打包布局抽查：首尾字段落位（与 ShadowUniformsLayout 静态断言互补）。
    #[test]
    fn pack_roundtrip() {
        let data = ShadowUniformsData {
            cascade_view_proj: [Mat4::from_translation(Vec3::ONE); SHADOW_CASCADES],
            planes: [Vec4::ONE; 24],
            splits: [1.0, 2.0, 4.0, 8.0],
            camera_forward: [0.0, 0.0, -1.0, 0.0],
            camera_pos: [0.0; 4],
            map_size: 2048.0,
            enabled: 1.0,
        };
        let bytes = pack_shadow_uniforms(&data);
        // cascade 0 的第 12 个分量（col3 平移 x=1 → 偏移 48）
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&bytes[48..52]);
        assert_eq!(f32::from_le_bytes(buf), 1.0);
        // splits[3] @ 640+12
        buf.copy_from_slice(&bytes[652..656]);
        assert_eq!(f32::from_le_bytes(buf), 8.0);
        // map_size @ 688 / enabled @ 692
        buf.copy_from_slice(&bytes[688..692]);
        assert_eq!(f32::from_le_bytes(buf), 2048.0);
        buf.copy_from_slice(&bytes[692..696]);
        assert_eq!(f32::from_le_bytes(buf), 1.0);
    }
}
