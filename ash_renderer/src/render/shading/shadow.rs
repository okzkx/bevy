//! 8.3 CSM 级联阴影宿主半边：级联矩阵计算、shadow UBO 打包、开关与诊断账面。
//!
//! 机制：
//!
//! - **级联划分**：4 级 PSSM 分割（log 与均匀按 [`SHADOW_SPLIT_LAMBDA`] 混合），
//!   覆盖 [`SHADOW_NEAR_DISTANCE`] → far；far = 场景 AABB 最远视深夹到
//!   [[`SHADOW_FAR_MIN`], [`SHADOW_FAR_MAX`]]，高机位俯瞰时整场景仍在带内。
//! - **XY 拟合 = 切片包围球**：球半径只取决于切片形状，相机转动/平移都不变，
//!   texel 世界尺寸恒定；盒中心在光空间按 texel 网格取整（世界原点锚定），
//!   盒只做整 texel 跳变，阴影边缘不爬行。正交光投影下投射体与它的影子落在
//!   同一 XY，XY 只需包住接收面（切片）。
//! - **深度向太阳侧延伸到场景顶**：比切片更靠近太阳的投射体（屋顶、高柱）
//!   必须在深度盒内，否则被近平面裁掉、也被 cull 平面剔掉 = 缺影。背日侧
//!   只到切片球底（再往下没有接收面）。
//! - **矩阵直构**：光空间基 (x, y, to_light)，ndc.z = 到盒底的光向距离 / 盒深
//!   ——越靠近太阳值越大，与主深度链 reverse-Z（清 0 + GREATER）同一方向。
//! - **bias 世界量纲**：UBO 附带逐级 texel 世界尺寸与盒深倒数，合成端的法线
//!   抬升、深度 bias、半影半径都按米算再折回 texel / NDC，换级联不失真。
//! - **平面族**：宿主 Gribb-Hartmann 从各 cascade_view_proj 提取六平面进
//!   shadow UBO（4×6=24 平面 384B 超 push 上限，故走 UBO）——`cull.wgsl`
//!   内联四 cascade 标记与本 CPU 参照同源同款。
//! - **时序**：shadow UBO 帧槽环（wait_for_slot fence 纪律）；GPU shadow
//!   标记诊断读数 = 本帧槽两帧前 compute 输出（与主标记同款）。

use bevy::math::{Mat4, Vec3, Vec4};
use bevy::prelude::Resource;

use crate::render::pipeline::shadow::SHADOW_UBO_SIZE;

/// 级联数量。
pub const SHADOW_CASCADES: usize = 4;
/// shadow map 边长（D32_SFLOAT，正方形）。
pub const SHADOW_MAP_SIZE: u32 = 2048;
/// 级联覆盖近端（米；主相机 near 以内不进 shadow 覆盖面）。
pub const SHADOW_NEAR_DISTANCE: f32 = 0.5;
/// 级联覆盖远端下限（米）：场景很小或无场景时也保证这么远有阴影。
pub const SHADOW_FAR_MIN: f32 = 80.0;
/// 级联覆盖远端上限（米）：防超大地面把末级 texel 拉到米级。
pub const SHADOW_FAR_MAX: f32 = 400.0;
/// PSSM 混合系数：1 = 纯 log（近级极窄），0 = 均匀（近处分辨率不足）。
pub const SHADOW_SPLIT_LAMBDA: f32 = 0.75;

/// 级间视深（升序，含首尾：`s[0]=near` … `s[4]=far`，长度 = 级数+1）。
/// split_i = λ·near·(far/near)^(i/n) + (1−λ)·(near + (far−near)·i/n)；
/// n=4 的 log 公比用双平方根展开。
#[must_use]
pub fn cascade_slice_boundaries(near: f32, far: f32) -> [f32; SHADOW_CASCADES + 1] {
    let step = (far / near).max(1.0).sqrt().sqrt();
    let mut bounds = [0.0_f32; SHADOW_CASCADES + 1];
    let mut log = near;
    for (i, slot) in bounds.iter_mut().enumerate() {
        let uniform = near + (far - near) * i as f32 / SHADOW_CASCADES as f32;
        *slot = SHADOW_SPLIT_LAMBDA * log + (1.0 - SHADOW_SPLIT_LAMBDA) * uniform;
        log *= step;
    }
    bounds[0] = near;
    bounds[SHADOW_CASCADES] = far;
    bounds
}

/// 级联矩阵计算的输入（draw_frame 组装，与帧 UBO 同源同帧）。
pub struct ShadowCascadeInput {
    /// inv(view_proj)：视深→NDC 反推切片角（与深度重建同源）。
    pub inv_view_proj: Mat4,
    /// 相机世界位置。
    pub camera_pos: Vec3,
    /// 相机 forward（单位）。
    pub camera_forward: Vec3,
    /// 投影 near（无限反向透视的 z_ndc↔视深换算）。
    pub near: f32,
    /// 指向光源的方向（= 灯实体 back()，与帧 UBO 同值）。
    pub dir_to_light: Vec3,
    /// 场景世界 AABB 并集（无可绘行 = None：far 取下限、深度不向场景顶延伸）。
    pub scene_aabb: Option<(Vec3, Vec3)>,
}

/// 级联拟合结果（逐级联数组，下标 = 级联序号）。
pub struct ShadowCascades {
    pub view_proj: [Mat4; SHADOW_CASCADES],
    /// 各级远端视深（合成端按视深选级）。
    pub splits: [f32; SHADOW_CASCADES],
    /// 一个 shadow texel 的世界边长（米）。
    pub texel_world: [f32; SHADOW_CASCADES],
    /// 1 / 光向盒深（米 → NDC 深度的换算）。
    pub inv_span: [f32; SHADOW_CASCADES],
}

/// 视深 → 无限反向透视的 NDC z（z_ndc = near/d，d ≤ near 时夹到 1）。
#[must_use]
pub fn view_depth_to_ndc_z(near: f32, d: f32) -> f32 {
    (near / d.max(near)).min(1.0)
}

/// 逐级联光空间正交拟合（机制见模块注释）。
#[must_use]
pub fn cascade_matrices(input: &ShadowCascadeInput) -> ShadowCascades {
    let to_light = if input.dir_to_light.length_squared() > 1e-8 {
        input.dir_to_light.normalize()
    } else {
        Vec3::Y
    };
    // 光空间右手基 (axis_x, axis_y, to_light)；光近铅直时换参考轴防退化
    let up_ref = if to_light.y.abs() > 0.99 { Vec3::X } else { Vec3::Y };
    let axis_x = up_ref.cross(to_light).normalize();
    let axis_y = to_light.cross(axis_x);

    // 场景角点：最远视深（定 far）与光向深度范围（定深度盒）
    let mut scene_far = 0.0_f32;
    let mut scene_depth: Option<(f32, f32)> = None;
    if let Some((mn, mx)) = input.scene_aabb {
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &x in &[mn.x, mx.x] {
            for &y in &[mn.y, mx.y] {
                for &z in &[mn.z, mx.z] {
                    let p = Vec3::new(x, y, z);
                    scene_far = scene_far.max((p - input.camera_pos).dot(input.camera_forward));
                    let d = to_light.dot(p);
                    lo = lo.min(d);
                    hi = hi.max(d);
                }
            }
        }
        scene_depth = Some((lo, hi));
    }
    let far = scene_far.clamp(SHADOW_FAR_MIN, SHADOW_FAR_MAX);
    let bounds = cascade_slice_boundaries(SHADOW_NEAR_DISTANCE, far);

    let mut out = ShadowCascades {
        view_proj: [Mat4::IDENTITY; SHADOW_CASCADES],
        splits: [0.0; SHADOW_CASCADES],
        texel_world: [0.0; SHADOW_CASCADES],
        inv_span: [0.0; SHADOW_CASCADES],
    };
    for c in 0..SHADOW_CASCADES {
        let (d0, d1) = (bounds[c], bounds[c + 1]);
        out.splits[c] = d1;
        // 切片 8 角点（NDC 满幅 × 两档视深）反投影到世界
        let mut corners = [Vec3::ZERO; 8];
        let mut k = 0;
        for &d in &[d0, d1] {
            let z = view_depth_to_ndc_z(input.near, d);
            for &x in &[-1.0_f32, 1.0] {
                for &y in &[-1.0_f32, 1.0] {
                    let p = input.inv_view_proj * Vec4::new(x, y, z, 1.0);
                    corners[k] = p.truncate() / p.w;
                    k += 1;
                }
            }
        }
        let center = corners.iter().copied().sum::<Vec3>() / 8.0;
        // 包围球半径取整到 1/16 米：浮点噪声不让 texel 尺寸逐帧抖动
        let radius = corners.iter().map(|p| p.distance(center)).fold(0.0_f32, f32::max);
        let radius = ((radius * 16.0).ceil() / 16.0).max(0.0625);
        let texel = 2.0 * radius / SHADOW_MAP_SIZE as f32;
        // 盒中心取整到 texel 网格（世界原点锚定）
        let cx = (axis_x.dot(center) / texel).round() * texel;
        let cy = (axis_y.dot(center) / texel).round() * texel;
        let cz = to_light.dot(center);
        // 深度盒：太阳侧到场景顶（投射体），背日侧到切片球底（接收面）
        let (mut z_bottom, mut z_top) = (cz - radius, cz + radius);
        if let Some((lo, hi)) = scene_depth {
            z_bottom = z_bottom.max(lo - 1.0);
            z_top = z_top.max(hi + 1.0);
        }
        let span = (z_top - z_bottom).max(0.1);
        let inv_half = 1.0 / radius;
        out.view_proj[c] = Mat4::from_cols(
            Vec4::new(axis_x.x * inv_half, axis_y.x * inv_half, to_light.x / span, 0.0),
            Vec4::new(axis_x.y * inv_half, axis_y.y * inv_half, to_light.y / span, 0.0),
            Vec4::new(axis_x.z * inv_half, axis_y.z * inv_half, to_light.z / span, 0.0),
            Vec4::new(-cx * inv_half, -cy * inv_half, -z_bottom / span, 1.0),
        );
        out.texel_world[c] = texel;
        out.inv_span[c] = 1.0 / span;
    }
    out
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
    /// 逐级联 texel 世界边长（米）。
    pub texel_world: [f32; SHADOW_CASCADES],
    /// 逐级联 1 / 光向盒深（米 → NDC 深度）。
    pub inv_span: [f32; SHADOW_CASCADES],
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
    texel_world: [f32; 4],
    inv_span: [f32; 4],
}

const _: () = {
    assert!(std::mem::offset_of!(ShadowUniformsLayout, planes) == 256);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, splits) == 640);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, camera_forward) == 656);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, camera_pos) == 672);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, map_size) == 688);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, texel_world) == 704);
    assert!(std::mem::offset_of!(ShadowUniformsLayout, inv_span) == 720);
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
    for (k, v) in d.texel_world.iter().enumerate() {
        put(704 + k * 4, &v.to_le_bytes());
    }
    for (k, v) in d.inv_span.iter().enumerate() {
        put(720 + k * 4, &v.to_le_bytes());
    }
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
    use bevy::math::Quat;

    fn project(m: &Mat4, p: Vec3) -> Vec3 {
        let clip = *m * p.extend(1.0);
        clip.truncate() / clip.w
    }

    /// 相机在 pos、按 rot 朝向的输入（fov 1 rad、方形视口）。
    fn camera_input(pos: Vec3, rot: Quat, dir_to_light: Vec3, scene_aabb: Option<(Vec3, Vec3)>) -> ShadowCascadeInput {
        let proj = PerspectiveProjection { fov: 1.0, aspect_ratio: 1.0, ..Default::default() };
        let view = Mat4::from_rotation_translation(rot, pos).inverse();
        ShadowCascadeInput {
            inv_view_proj: (proj.get_clip_from_view() * view).inverse(),
            camera_pos: pos,
            camera_forward: rot * Vec3::NEG_Z,
            near: proj.near,
            dir_to_light,
            scene_aabb,
        }
    }

    /// 各级切片 8 角点都落在本级 NDC 盒内（XY ∈ [-1,1]、深度 ∈ [0,1]），
    /// 分割带单调且首尾正确。
    #[test]
    fn slices_fit_inside_cascades() {
        let light = Vec3::new(0.3, 1.0, 0.2).normalize();
        let input = camera_input(Vec3::new(1.0, 2.0, 3.0), Quat::from_rotation_y(0.7), light, None);
        let fit = cascade_matrices(&input);
        let bounds = cascade_slice_boundaries(SHADOW_NEAR_DISTANCE, SHADOW_FAR_MIN);
        assert!((fit.splits[SHADOW_CASCADES - 1] - SHADOW_FAR_MIN).abs() < 1e-4);
        for w in fit.splits.windows(2) {
            assert!(w[0] < w[1], "分割带必须升序: {:?}", fit.splits);
        }
        for c in 0..SHADOW_CASCADES {
            for &d in &[bounds[c], bounds[c + 1]] {
                let z = view_depth_to_ndc_z(input.near, d);
                for &x in &[-1.0_f32, 1.0] {
                    for &y in &[-1.0_f32, 1.0] {
                        let p = input.inv_view_proj * Vec4::new(x, y, z, 1.0);
                        let ndc = project(&fit.view_proj[c], p.truncate() / p.w);
                        assert!(ndc.x.abs() <= 1.001 && ndc.y.abs() <= 1.001, "级联 {c} 角点出 XY 盒: {ndc}");
                        assert!((-1e-4..=1.0001).contains(&ndc.z), "级联 {c} 角点出深度盒: {ndc}");
                    }
                }
            }
            assert!(fit.texel_world[c] > 0.0 && fit.inv_span[c] > 0.0);
        }
    }

    /// 遮挡方向：太阳在正上方时，切片上方 30m 的投射体与地面接收点同 XY，
    /// 投射体深度值更大（reverse-Z：更近太阳 = 更大）且仍在深度盒内——
    /// 投射体不能被近平面裁掉。
    #[test]
    fn tall_caster_toward_sun_stays_in_box() {
        let aabb = (Vec3::new(-20.0, 0.0, -60.0), Vec3::new(20.0, 30.0, 5.0));
        let input = camera_input(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY, Vec3::Y, Some(aabb));
        let fit = cascade_matrices(&input);
        let receiver = Vec3::new(0.0, 0.0, -3.0);
        let caster = Vec3::new(0.0, 29.0, -3.0);
        let c = fit.splits.iter().position(|&s| 3.0 <= s).expect("视深 3m 有级联");
        let (r, k) = (project(&fit.view_proj[c], receiver), project(&fit.view_proj[c], caster));
        assert!((r.x - k.x).abs() < 1e-4 && (r.y - k.y).abs() < 1e-4, "同一光线应同 XY: {r} {k}");
        assert!(k.z > r.z, "投射体必须比接收点更近太阳: caster {k} receiver {r}");
        assert!((0.0..=1.0).contains(&k.z) && (0.0..=1.0).contains(&r.z));
    }

    /// 高机位俯瞰：场景远角视深超 80m 时末级远端跟着外扩，但不超上限。
    #[test]
    fn far_follows_scene_extent() {
        let aabb = (Vec3::new(-100.0, 0.0, -150.0), Vec3::new(100.0, 10.0, 0.0));
        let input = camera_input(Vec3::new(0.0, 60.0, 20.0), Quat::from_rotation_x(-0.6), Vec3::Y, Some(aabb));
        let far = cascade_matrices(&input).splits[SHADOW_CASCADES - 1];
        assert!(far > SHADOW_FAR_MIN && far <= SHADOW_FAR_MAX, "far = {far}");
    }

    /// 稳定性：相机平移不改变固定世界点在 texel 内的相位（盒只整 texel 跳），
    /// 相机转动不改变 texel 世界尺寸（包围球半径与朝向无关）。
    #[test]
    fn texel_grid_stable_under_camera_motion() {
        let light = Vec3::new(0.4, 0.8, -0.3).normalize();
        let aabb = Some((Vec3::new(-30.0, 0.0, -30.0), Vec3::new(30.0, 15.0, 30.0)));
        let a = cascade_matrices(&camera_input(Vec3::ZERO, Quat::IDENTITY, light, aabb));
        let b = cascade_matrices(&camera_input(Vec3::new(0.37, 0.11, -0.53), Quat::IDENTITY, light, aabb));
        let rot = cascade_matrices(&camera_input(Vec3::ZERO, Quat::from_rotation_y(1.1), light, aabb));
        let phase = |v: f32| {
            let t = (v + 1.0) * 0.5 * SHADOW_MAP_SIZE as f32;
            t - t.floor()
        };
        for c in 0..SHADOW_CASCADES {
            assert_eq!(a.texel_world[c], b.texel_world[c], "级联 {c} 平移改了 texel 尺寸");
            assert_eq!(a.texel_world[c], rot.texel_world[c], "级联 {c} 转动改了 texel 尺寸");
            let world = Vec3::new(0.2, 0.0, -0.5 * (a.splits[c] + if c == 0 { 0.5 } else { a.splits[c - 1] }));
            let (na, nb) = (project(&a.view_proj[c], world), project(&b.view_proj[c], world));
            for (pa, pb) in [(phase(na.x), phase(nb.x)), (phase(na.y), phase(nb.y))] {
                let diff = (pa - pb).abs();
                assert!(diff.min(1.0 - diff) < 1e-2, "级联 {c} texel 相位漂移: {pa} {pb}");
            }
        }
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
            texel_world: [0.5, 1.0, 2.0, 4.0],
            inv_span: [0.25, 0.2, 0.1, 0.05],
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
        // texel_world[3] @ 704+12 / inv_span[0] @ 720
        buf.copy_from_slice(&bytes[716..720]);
        assert_eq!(f32::from_le_bytes(buf), 4.0);
        buf.copy_from_slice(&bytes[720..724]);
        assert_eq!(f32::from_le_bytes(buf), 0.25);
    }
}
