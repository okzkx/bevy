//! 相机组（业务半边）：裸 Camera 三件套的 spawn 与就位核验，取景参数与官方
//! 对照进程同源，零 Vulkan 代码。
//!
//! 宽高比补位是通用机制，住 [`crate::scene::mechanism::camera_aspect`]；
//! "为什么裸 `Camera` 不用 `Camera3d`"的机制见
//! `.agents/docs/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use bevy::{
    camera::{Camera, Projection},
    prelude::*,
    window::PrimaryWindow,
};

use ash_macros::system;

use crate::scene::mechanism::CameraOrbit;
use crate::scene::util::fmt_vec3;

/// 相机取景参数：与官方对照进程（examples/official_reference.rs）用同一组
/// 参数——并排对照时几何与光照方向判定才同源可比。
const CAMERA_POS: Vec3 = Vec3::new(0.7, 0.7, 1.0);
const CAMERA_TARGET: Vec3 = Vec3::new(0.0, 0.3, 0.0);

/// 相机进场（Startup）：裸 [`Camera`] + [`Projection`] + [`Transform`]。
///
/// 相机用裸 [`Camera`] 而非 `Camera3d`：后者携带渲染图、管纹理用法等渲染族死重，
/// 我们只消费 Camera+Projection+Transform 三样数据。"无渲染图的 Camera 运行时
/// 会报错"（bevy_camera/src/camera.rs:368-371）——报错者是渲染侧系统，禁渲染后
/// 无人报。
#[system]
pub(super) fn spawn_camera(
    mut commands: Commands,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    // 宽高比初值：官方由 camera_system（bevy_render/src/camera.rs:354，渲染族已禁）
    // 随窗口建/改维护，禁后归我们——Startup 先按主窗口写一次，后续 resize 由
    // 机制侧宽高比补位（crate::scene::mechanism::camera_aspect）接管。宽高比错了，
    // 建 VP 矩阵时横向视野就错。
    let mut projection = Projection::default();
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
        Transform::from_xyz(CAMERA_POS.x, CAMERA_POS.y, CAMERA_POS.z)
            .looking_at(CAMERA_TARGET, Vec3::Y),
        // 轨道控制状态随相机进场（3.9）：环绕目标 = 取景注视点，机位零跳变
        // 反解为球坐标（机制半边 AshCameraControlPlugin 逐帧驱动）。
        CameraOrbit::from_pose(CAMERA_POS, CAMERA_TARGET),
    ));
    info!(
        "相机进场：Camera+Projection+Transform @ {} 看向 {}（官方示例取景，宽高比{aspect_note}）",
        fmt_vec3(CAMERA_POS),
        fmt_vec3(CAMERA_TARGET),
    );
}

/// 相机就位核验（Update，报一次即歇）：相机 1 台且 GlobalTransform 前向
/// 精确指向 [`CAMERA_TARGET`]——相机是根实体、无父链，Transform require 的
/// GlobalTransform 种子值即终值，looking_at 语义逐字成立。带父链的传播验证
/// 与逐帧采集归机制侧采集（crate::scene::mechanism::collect）。
#[derive(Default)]
pub(super) struct CameraSetupState {
    done: bool,
}

#[system]
pub(super) fn report_camera_setup(
    mut state: Local<CameraSetupState>,
    cameras: Query<(&GlobalTransform, &Projection), With<Camera>>,
) {
    if state.done {
        return;
    }
    state.done = true;
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
    // 判定线：前向与"位置→注视点"夹角应近 0（浮点级）；超 1° 说明朝向语义被破坏
    let expected = (CAMERA_TARGET - CAMERA_POS).normalize_or_zero();
    let deviation = (*cam_tf.forward()).angle_between(expected).to_degrees();
    info!(
        "相机就位：1 台 @ {} 前向 {} 与位置→注视点夹角 {deviation:.4}°（looking_at 语义），{aspect}",
        fmt_vec3(cam_tf.translation()),
        fmt_vec3(*cam_tf.forward()),
    );
}
