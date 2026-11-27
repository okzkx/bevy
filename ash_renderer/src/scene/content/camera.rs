//! 相机组（业务侧）：裸 Camera 三件套的 spawn 与就位核验，取景参数与官方
//! 对照进程同源，零 Vulkan 代码。
//!
//! 宽高比补位由机制层的 [`crate::scene::AshCameraAspectPlugin`] 提供；
//! "为什么裸 `Camera` 不用 `Camera3d`"的机制见
//! `.agents/docs/开发步骤/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use bevy::{
    camera::{Camera, Projection},
    mesh::{Mesh, Mesh3d, VertexAttributeValues},
    prelude::*,
    window::PrimaryWindow,
};

use ash_macros::system;

use super::fmt_vec3;
use crate::scene::mechanism::{CameraOrbit, CameraOrbitCommand, apply_orbit_command};

/// 相机取景参数：Sponza 中庭机位（彩旗拱廊狮头喷泉全景）。Sponza 世界 AABB ≈
/// 30×12.4×18.3 米（节点 scale 0.008），中庭中心 ≈ (0, 3, 0)：机位在
/// 中庭东端沿长轴向西看（球坐标 yaw 90°/pitch 5°/radius 5 的直角反解）。
/// `--scene` 指定其他场景时机位不适配（可能埋进几何里），用 WASDQE 漫游/
/// 右键环视或 BRP `set_camera` 调整。
const CAMERA_POS: Vec3 = Vec3::new(4.98, 3.44, 0.0);
const CAMERA_TARGET: Vec3 = Vec3::new(0.0, 3.0, 0.0);

/// 相机进场（Startup）：裸 [`Camera`] + [`Projection`] + [`Transform`]。
///
/// 不用 `Camera3d`：它带渲染图等渲染族组件，这里只消费 Camera+Projection+Transform；
/// "无渲染图的 Camera 会报错"出自渲染侧系统，禁渲染后不触发。
#[system]
pub(super) fn spawn_camera(mut commands: Commands, windows: Query<&Window, With<PrimaryWindow>>) {
    // 宽高比初值：渲染族禁用后官方不再维护，Startup 先按主窗口写一次，resize 由
    // 机制侧宽高比同步接管。宽高比错了，VP 矩阵的横向视野就错。
    let mut projection = Projection::default();
    if let Projection::Perspective(ref mut persp) = projection {
        // 远平面放宽：默认 1000 不够俯瞰 256+ 单位的常驻大场景。坑：resize 补位只改宽高比，
        // 远平面必须在这里无条件设，不能放进"主窗口在场"分支
        persp.far = 4000.0;
    }
    let mut aspect_note = "默认 1.0，交 Update 修正";
    if let Ok(window) = windows.single()
        && window.resolution.width() > 0.0
        && window.resolution.height() > 0.0
        && let Projection::Perspective(ref mut persp) = projection
    {
        persp.aspect_ratio = window.resolution.width() / window.resolution.height();
        aspect_note = "按主窗口";
    }
    commands.spawn((
        Camera::default(),
        projection,
        Transform::from_translation(CAMERA_POS).looking_at(CAMERA_TARGET, Vec3::Y),
        // 轨道状态由进场机位反解，环绕目标 = 注视点，接管时机位零跳变。
        CameraOrbit::from_pose(CAMERA_POS, CAMERA_TARGET),
    ));
    info!(
        "相机进场：Camera+Projection+Transform @ {} 看向 {}（Sponza 中庭取景，宽高比{aspect_note}）",
        fmt_vec3(CAMERA_POS),
        fmt_vec3(CAMERA_TARGET),
    );
}

/// 相机就位核验（Update，只跑一次）：相机 1 台且 GlobalTransform 前向
/// 精确指向 [`CAMERA_TARGET`]——相机是无父链的根实体，GlobalTransform 种子值
/// 即终值。带父链的传播验证归机制侧账本。
#[system]
pub(super) fn report_camera_setup(cameras: Query<(&GlobalTransform, &Projection), With<Camera>>) {
    let Ok((cam_tf, projection)) = cameras.single() else {
        warn!(
            "相机就位核验未过：相机 {} 台（期望 1）",
            cameras.iter().count(),
        );
        return;
    };
    let aspect = if let Projection::Perspective(persp) = projection {
        format!("宽高比 {:.4}", persp.aspect_ratio)
    } else {
        "非透视投影（意外）".to_string()
    };
    // 前向与"位置→注视点"夹角应近 0（浮点级）；超 1° 说明朝向被破坏，升 warn
    let expected = (CAMERA_TARGET - CAMERA_POS).normalize_or_zero();
    let deviation = (*cam_tf.forward()).angle_between(expected).to_degrees();
    if deviation > 1.0 {
        warn!("相机朝向核验越线：前向与位置→注视点夹角 {deviation:.4}° > 1°（朝向语义被破坏）");
    }
    info!(
        "相机就位：1 台 @ {} 前向 {} 与位置→注视点夹角 {deviation:.4}°（looking_at 语义），{aspect}",
        fmt_vec3(cam_tf.translation()),
        fmt_vec3(*cam_tf.forward()),
    );
}

/// `--scene` 覆盖时的自动机位状态：进场请求时由 `load_scene` 置位，摆完即清。
///
/// 为什么需要：默认机位按 Sponza 中庭反解（target 3 m 高、radius 5 m），对 0.6 m
/// 量级的材质测试场景会直接飞到天上；这里按实际进场几何的包围盒重新取景。
#[derive(Resource, Default)]
pub(super) struct SceneAutoFrame {
    pending: bool,
    /// 上一帧的 primitive 数：等实体树稳定一帧再算包围盒（WorldAsset 是一次
    /// 反射展开，通常一帧到齐，留一帧余量防半成品）。
    last_count: usize,
}

impl SceneAutoFrame {
    /// 请求一次自动取景（只在 `--scene` 覆盖时调用）。
    pub(super) fn request() -> Self {
        Self {
            pending: true,
            last_count: 0,
        }
    }
}

/// 测试场景自动机位（Update）：等 primitive 数量稳定后按世界包围盒摆轨道机位。
///
/// 取景口径：中心 = 包围盒中心；距离 = 包围球半径 / tan(fov_y/2) × 1.25 余量；
/// 俯角 30°、yaw 0（与官方材质测试场景的阅读方向一致）。Sponza 默认机位不走这里。
#[system]
pub(super) fn auto_frame_scene(
    mut state: ResMut<SceneAutoFrame>,
    meshes: Res<Assets<Mesh>>,
    primitives: Query<(&GlobalTransform, &Mesh3d)>,
    mut cameras: Query<(&mut Transform, &mut CameraOrbit, &Camera, &Projection)>,
) {
    if !state.pending {
        return;
    }
    let count = primitives.iter().count();
    if count == 0 || count != state.last_count {
        state.last_count = count;
        return;
    }
    let Ok((mut transform, mut orbit, _camera, projection)) = cameras.single_mut() else {
        warn!("测试场景自动机位未摆：相机不是恰好 1 台");
        state.pending = false;
        return;
    };
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for (tf, mesh3d) in &primitives {
        let Some(mesh) = meshes.get(&mesh3d.0) else {
            continue;
        };
        let Some(VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            continue;
        };
        for p in positions {
            let world = tf.transform_point(Vec3::from_array(*p));
            min = min.min(world);
            max = max.max(world);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        warn!("测试场景自动机位未摆：进场几何没有可用 POSITION");
        state.pending = false;
        return;
    }
    let center = (min + max) * 0.5;
    let diagonal = (max - min).length();
    let fov_y = match projection {
        Projection::Perspective(persp) => persp.fov,
        _ => 45.0_f32.to_radians(),
    };
    // `f32::tan` 被 clippy.toml 的 disallowed-methods 拦下（libm 确定性），统一走 bevy_math::ops。
    let radius = (0.5 * diagonal / bevy::math::ops::tan(fov_y * 0.5)).max(0.05) * 1.25;
    let cmd = CameraOrbitCommand {
        target: Some(center),
        yaw_deg: Some(0.0),
        pitch_deg: Some(30.0),
        radius: Some(radius),
        zoom: None,
    };
    apply_orbit_command(&mut transform, &mut orbit, &cmd);
    state.pending = false;
    info!(
        "测试场景自动机位：包围盒 {} → {}（对角线 {diagonal:.3} m），target {} radius {radius:.3}",
        fmt_vec3(min),
        fmt_vec3(max),
        fmt_vec3(center),
    );
}
