//! 相机组（施工 3.1.3）：裸 Camera 三件套的 spawn、宽高比补位与就位核验，零 Vulkan 代码。
//!
//! 为什么裸 `Camera` 不用 `Camera3d`、宽高比为什么归我们补位——机制见
//! `.agent/docs/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`。

use bevy::{
    camera::{Camera, Projection},
    prelude::*,
    window::PrimaryWindow,
};

use super::util::fmt_vec3;

/// 相机取景参数（抄官方 FlightHelmet 示例 examples/3d/anti_aliasing.rs `setup`）：
/// 3.5 同屏对照时 bevy wgpu 侧用同一组参数，几何与光照方向判定才同源可比。
const CAMERA_POS: Vec3 = Vec3::new(0.7, 0.7, 1.0);
const CAMERA_TARGET: Vec3 = Vec3::new(0.0, 0.3, 0.0);

/// 相机进场（Startup）：裸 [`Camera`] + [`Projection`] + [`Transform`]，官方示例取景。
///
/// 相机用裸 [`Camera`] 而非 `Camera3d`：后者携带渲染图、管纹理用法等渲染族死重，
/// 我们只消费 Camera+Projection+Transform 三样数据。"无渲染图的 Camera 运行时
/// 会报错"（bevy_camera/src/camera.rs:368-371）——报错者是渲染侧系统，禁渲染后
/// 无人报，正合"数据先行、渲染后置"节奏。
pub(super) fn spawn_camera(
    mut commands: Commands,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    // 宽高比初值：官方由 camera_system（bevy_render/src/camera.rs:354，渲染族已禁）
    // 随窗口建/改维护，禁后归我们——Startup 先按主窗口写一次，后续 resize 由
    // [`sync_projection_aspect`] 接管。宽高比错了，3.4 建 VP 矩阵时横向视野就错。
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
    ));
    info!(
        "相机进场：Camera+Projection+Transform @ {} 看向 {}（官方示例取景，宽高比{aspect_note}）",
        fmt_vec3(CAMERA_POS),
        fmt_vec3(CAMERA_TARGET),
    );
}

/// 宽高比补位（Update，幂等对比-修正）：camera_system 缺席后没人随窗口 resize
/// 更新 `PerspectiveProjection.aspect_ratio`（初值 1.0），不补位则 3.4 的画面
/// 横向拉伸。宽度/高度为 0（最小化）跳过，等恢复。
pub(super) fn sync_projection_aspect(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut cameras: Query<&mut Projection, With<Camera>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let (w, h) = (window.resolution.width(), window.resolution.height());
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let aspect = w / h;
    for mut projection in &mut cameras {
        if let Projection::Perspective(ref mut persp) = *projection
            && (persp.aspect_ratio - aspect).abs() > f32::EPSILON
        {
            let old = persp.aspect_ratio;
            persp.aspect_ratio = aspect;
            debug!("相机宽高比随窗口修正：{old:.4} → {aspect:.4}");
        }
    }
}

/// 相机就位核验（Update，报一次即歇）：相机 1 台且 GlobalTransform 前向
/// 精确指向 [`CAMERA_TARGET`]——相机是根实体、无父链，Transform require 的
/// GlobalTransform 种子值即终值，looking_at 语义逐字成立。带父链的传播验证
/// 与逐帧采集随 3.1.4 兑现。
#[derive(Default)]
pub(super) struct CameraSetupState {
    done: bool,
}

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
