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

/// 相机取景参数：Sponza 中庭机位（彩旗拱廊狮头喷泉全景）。Sponza 世界 AABB ≈
/// 30×12.4×18.3 米（节点 scale 0.008），中庭中心 ≈ (0, 3, 0)：机位在
/// 中庭东端沿长轴向西看（球坐标 yaw 90°/pitch 5°/radius 5 的直角反解）。
/// `--scene` 指定其他场景时机位不适配（可能埋进几何里），用 WASDQE 漫游/
/// 右键环视或 BRP `set_camera` 调整。
const CAMERA_POS: Vec3 = Vec3::new(4.98, 3.44, 0.0);
const CAMERA_TARGET: Vec3 = Vec3::new(0.0, 3.0, 0.0);

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
        // 远平面放宽：默认 1000 不够俯瞰 256+ 单位的常驻大场景
        persp.far = 4000.0;
        aspect_note = "按主窗口";
    }
    commands.spawn((
        Camera::default(),
        projection,
        Transform::from_xyz(CAMERA_POS.x, CAMERA_POS.y, CAMERA_POS.z)
            .looking_at(CAMERA_TARGET, Vec3::Y),
        // 轨道控制状态随相机进场：环绕目标 = 取景注视点，机位零跳变
        // 反解为球坐标（机制半边 AshCameraControlPlugin 逐帧驱动）。
        CameraOrbit::from_pose(CAMERA_POS, CAMERA_TARGET),
    ));
    info!(
        "相机进场：Camera+Projection+Transform @ {} 看向 {}（Sponza 中庭取景，宽高比{aspect_note}）",
        fmt_vec3(CAMERA_POS),
        fmt_vec3(CAMERA_TARGET),
    );
}

/// 相机就位核验（Update，报一次即歇）：相机 1 台且 GlobalTransform 前向
/// 精确指向 [`CAMERA_TARGET`]——相机是根实体、无父链，Transform require 的
/// GlobalTransform 种子值即终值，looking_at 语义逐字成立。带父链的传播验证
/// 与逐帧矩阵更新归机制侧账本（update_ledger 的 Changed<GlobalTransform> 通道）。
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
    //（越线升 warn：对齐同文件另两处核验的 Tier① 口径，不静默 info）
    let expected = (CAMERA_TARGET - CAMERA_POS).normalize_or_zero();
    let deviation = (*cam_tf.forward()).angle_between(expected).to_degrees();
    if deviation > 1.0 {
        warn!(
            "相机朝向核验越线：前向与位置→注视点夹角 {deviation:.4}° > 1°（朝向语义被破坏）"
        );
    }
    info!(
        "相机就位：1 台 @ {} 前向 {} 与位置→注视点夹角 {deviation:.4}°（looking_at 语义），{aspect}",
        fmt_vec3(cam_tf.translation()),
        fmt_vec3(*cam_tf.forward()),
    );
}
