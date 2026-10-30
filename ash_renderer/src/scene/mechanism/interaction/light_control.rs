//! 中键拖动控制方向光；状态由灯光实体上的 [`SunOrbit`] 保存，BRP 与拖拽共用写回路径。
//! Update 写 Transform，传播后由 Last 的帧循环读取；输入仲裁由 PointerCapture 提供。
//! 坑：旋转轴取相机右/上轴；绕世界 Y 会使俯视时的拖动接近无效。
//! 不抓取指针；拖出窗口时没有增量，光标重新进入时可能产生较大位移。

use bevy::{
    light::DirectionalLight,
    prelude::*,
    math::ops,
    window::{PrimaryWindow, Window},
    input::{mouse::MouseButton, ButtonInput},
};

use super::camera_control::{DragSession, FULL_TURN_PER_HEIGHT};
use super::pointer_capture::{PointerCapture, PointerCaptureSet};

use ash_macros::system;

/// 仰角限制为地平线至距天顶 1°；避免 `look_at` 的 up=Y 退化。
const MIN_SUN_ELEVATION: f32 = 0.0;
const MAX_SUN_ELEVATION: f32 = 89.0f32.to_radians();

/// 将仰角限制在地平线与天顶余量之间。
pub fn clamp_elevation(elevation: f32) -> f32 {
    elevation.clamp(MIN_SUN_ELEVATION, MAX_SUN_ELEVATION)
}

/// 方向光实体携带的太阳方位角与仰角状态。
#[derive(Component, Debug, Clone, Copy)]
pub struct SunOrbit {
    /// 太阳方位角（rad，绕 +Y）：0 = 太阳在 +Z 侧，增加 = +Z 向 +X（俯视顺时针）。
    pub azimuth: f32,
    /// 太阳仰角（rad）：0 = 在地平线，上限 [`MAX_SUN_ELEVATION`]。
    pub elevation: f32,
}

impl SunOrbit {
    /// 从场景指向太阳的方向反解球坐标；零向量回落为 +Z 侧地平线。
    pub fn from_dir(sun_dir: Vec3) -> Self {
        let d = sun_dir.normalize_or_zero();
        Self {
            azimuth: ops::atan2(d.x, d.z),
            elevation: ops::asin(d.y.clamp(-1.0, 1.0)),
        }
    }

    pub(crate) fn sun_dir(&self) -> Vec3 {
        let (sin_az, cos_az) = ops::sin_cos(self.azimuth);
        let (sin_el, cos_el) = ops::sin_cos(self.elevation);
        Vec3::new(sin_az * cos_el, sin_el, cos_az * cos_el)
    }
}

/// 按相机右轴和上轴旋转太阳方向，再回写球坐标并限制仰角。
fn light_rotate(orbit: &mut SunOrbit, delta: Vec2, turn: f32, cam_right: Vec3, cam_up: Vec3) {
    let d = orbit.sun_dir();
    let d = Quat::from_axis_angle(cam_up, delta.x * turn) * d;
    let d = Quat::from_axis_angle(cam_right, delta.y * turn) * d;
    *orbit = SunOrbit::from_dir(d);
    orbit.elevation = orbit.elevation.clamp(MIN_SUN_ELEVATION, MAX_SUN_ELEVATION);
}

/// 方向光沿实体 forward 照射，因此朝向设为太阳方向的反向。
pub fn apply_sun_rotation(orbit: &SunOrbit, transform: &mut Transform) {
    transform.look_at(-orbit.sun_dir(), Vec3::Y);
}

/// 注册日光控制系统及指针仲裁资源。
pub struct AshLightControlPlugin;

impl Plugin for AshLightControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PointerCapture>()
            .add_systems(Update, light_control.after(PointerCaptureSet));
    }
}

/// Update 中处理日光拖动并将方向写回 Transform；只有按下中键时检查 UI 悬停。
#[system]
fn light_control(
    windows: Query<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    capture: Res<PointerCapture>,
    cams: Query<&GlobalTransform, With<Camera>>,
    mut lights: Query<(&mut Transform, &mut SunOrbit), With<DirectionalLight>>,
    mut session: Local<DragSession>,
    mut engaged: Local<bool>,
) {
    // 主窗口缺席或最小化时，取消拖拽会话。
    let Ok(window) = windows.single() else {
        *session = DragSession::default();
        return;
    };
    if window.width() <= 0.0 || window.height() <= 0.0 {
        *session = DragSession::default();
        return;
    }
    let cursor = window.cursor_position();

    if mouse.just_pressed(MouseButton::Middle) && !capture.over_ui {
        session.dragging = cursor.is_some();
        session.last_cursor = cursor;
    }
    if mouse.just_released(MouseButton::Middle) {
        session.dragging = false;
        session.last_cursor = None;
    }

    // 中键拖动按窗口高度归一；旋转轴使用上一帧传播的相机右轴和上轴。
    if session.dragging
        && mouse.pressed(MouseButton::Middle)
        && let Some(pos) = cursor
    {
        if let Some(last) = session.last_cursor {
            let delta = pos - last;
            let turn = FULL_TURN_PER_HEIGHT / window.height();
            let (cam_right, cam_up) = cams
                .iter()
                .next()
                .map_or((Vec3::X, Vec3::Y), |tf| (*tf.right(), *tf.up()));
            for (transform, orbit) in &mut lights {
                let transform = transform.into_inner();
                let orbit = orbit.into_inner();
                light_rotate(orbit, delta, turn, cam_right, cam_up);
                apply_sun_rotation(orbit, transform);
            }
        }
        session.last_cursor = Some(pos);
    }

    if !*engaged
        && mouse.pressed(MouseButton::Middle)
        && let Ok((_, orbit)) = lights.single()
    {
        *engaged = true;
        info!(
            "日光控制首次接管：太阳方位 {:.1}° / 仰角 {:.1}°（中键拖拽旋转日光已生效）",
            orbit.azimuth.to_degrees(),
            orbit.elevation.to_degrees(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 球坐标 ↔ 方向往返：from_dir 反解后 sun_dir 复原同一方向。
    #[test]
    fn from_dir_sun_dir_roundtrip() {
        let dir = Vec3::new(0.405, 0.206, 0.891);
        let orbit = SunOrbit::from_dir(dir);
        let back = orbit.sun_dir();
        assert!(
            (back - dir.normalize()).length() < 1e-4,
            "往返应复原方向，实际 {back:?}"
        );
    }

    /// 平视相机下向右拖动，太阳沿屏幕右侧移动且仰角不变。
    #[test]
    fn drag_right_moves_sun_screen_right_level_camera() {
        let mut orbit = SunOrbit { azimuth: 0.0, elevation: 0.5 };
        let d0 = orbit.sun_dir();
        light_rotate(&mut orbit, Vec2::new(100.0, 0.0), 0.01, Vec3::X, Vec3::Y);
        let d1 = orbit.sun_dir();
        assert!((d1 - d0).dot(Vec3::X) > 0.0, "右拖太阳应向屏右移动");
        assert!((orbit.elevation - 0.5).abs() < 1e-4, "水平拖不该动仰角");
    }

    /// 平视相机下拖 = 太阳降低，且夹在地平线不再往下。
    #[test]
    fn drag_down_lowers_sun_clamped_at_horizon() {
        let mut orbit = SunOrbit { azimuth: 0.0, elevation: 0.3 };
        light_rotate(&mut orbit, Vec2::new(0.0, 50.0), 0.01, Vec3::X, Vec3::Y);
        assert!(orbit.elevation < 0.3, "下拖仰角应下降，实际 {}", orbit.elevation);
        light_rotate(&mut orbit, Vec2::new(0.0, 100.0), 0.01, Vec3::X, Vec3::Y);
        assert_eq!(orbit.elevation, MIN_SUN_ELEVATION, "仰角应夹在地平线");
    }

    /// 俯视相机右拖时，天顶附近的太阳仍应向屏幕右侧移动。
    #[test]
    fn drag_right_moves_sun_screen_right_topdown_camera() {
        let mut orbit = SunOrbit { azimuth: 0.0, elevation: 1.4 };
        let d0 = orbit.sun_dir();
        light_rotate(&mut orbit, Vec2::new(100.0, 0.0), 0.01, Vec3::X, -Vec3::Z);
        let d1 = orbit.sun_dir();
        assert!(
            (d1 - d0).dot(Vec3::X) > 0.0,
            "俯视机位右拖太阳应向屏右移动，d0={d0:?} d1={d1:?}"
        );
    }

    /// 写回朝向：灯光 forward = 太阳方向的反向（沿 forward 照射语义）。
    #[test]
    fn apply_rotation_points_forward_away_from_sun() {
        let orbit = SunOrbit { azimuth: 0.5, elevation: 0.4 };
        let mut transform = Transform::IDENTITY;
        apply_sun_rotation(&orbit, &mut transform);
        let forward = *transform.forward();
        let expected = -orbit.sun_dir();
        assert!(
            (forward - expected.normalize()).length() < 1e-4,
            "forward 应背对太阳，实际 {forward:?} 期望 {expected:?}"
        );
    }
}
