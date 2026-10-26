//! 日光方向控制（机制半边）：中键拖拽旋转方向光——屏幕系"穹顶拖拽"：右拖 =
//! 太阳向屏幕右转、下拖 = 向屏幕下降，旋转轴取相机右/上轴而非世界 Y，俯视等
//! 非常规机位下太阳仍跟随光标方向走。零 Vulkan 代码，也不含写死的场景参数。
//! 与 [`super::camera_control`] 同一套输入形态：跨帧拖拽会话（[`DragSession`]）
//! + UI 指针仲裁（[`PointerCapture`]，悬在调试面板上让给 UI）。
//!
//! 状态真源 = 方向光实体携带的 [`SunOrbit`] 球坐标组件（方位角/仰角），业务
//! 半边（[`crate::scene::content::lights`]）进场时从初始朝向反解挂上，机制
//! 半边只在中键拖出增量时写 [`Transform`]——与相机控制同一纪律：拖拽前
//! 渲染语义零变化，朝向恒由球坐标重导出（look_at，up=Y）。BRP
//! `sun_get`/`sun_set` 与拖拽共用这一条"球坐标 → 朝向"通路。
//!
//! 帧内时序与相机控制同款：Update 改 Transform → PostUpdate 传播 → Last 的
//! draw_frame 直读 GlobalTransform（方向光沿实体 forward 照射，无位置语义）。
//!
//! 局限：与左右键拖拽一样不做指针抓取，光标拖出窗口即无增量（会话保活、
//! 回窗续拖，防跳变同款）；仰角下限钉在地平线（日光语义 = 太阳在天上）；
//! 拖拽旋转轴取相机的 GlobalTransform（上一帧传播结果，滞后一帧无感）。

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

/// 仰角限位：下限 = 地平线（日光不照地下）；上限距天顶留 1° 余量，
/// `sun_dir()` 与 Y 不平行，look_at 的 up=Y 才不退化。BRP `sun_set` 同一限位。
const MIN_SUN_ELEVATION: f32 = 0.0;
const MAX_SUN_ELEVATION: f32 = 89.0f32.to_radians();

/// 仰角限位（BRP 写入与拖拽共用）：地平线到天顶余量。
pub fn clamp_elevation(elevation: f32) -> f32 {
    elevation.clamp(MIN_SUN_ELEVATION, MAX_SUN_ELEVATION)
}

/// 日光方向状态：太阳方位角/仰角（球坐标，世界系）。方向光实体携带，机制
/// 半边中键驱动写 Transform；初始值由业务半边从进场朝向反解（[`from_dir`]），
/// 拖拽前前向零跳变。
#[derive(Component, Debug, Clone, Copy)]
pub struct SunOrbit {
    /// 太阳方位角（rad，绕 +Y）：0 = 太阳在 +Z 侧，增加 = +Z 向 +X（俯视顺时针）。
    pub azimuth: f32,
    /// 太阳仰角（rad）：0 = 在地平线，上限 [`MAX_SUN_ELEVATION`]。
    pub elevation: f32,
}

impl SunOrbit {
    /// 从太阳方向（场景指向太阳，= 灯光 forward 的反向）反解球坐标；
    /// 非归一方向就地归一，零向量回落到正东地平线。
    pub fn from_dir(sun_dir: Vec3) -> Self {
        let d = sun_dir.normalize_or_zero();
        Self {
            azimuth: ops::atan2(d.x, d.z),
            elevation: ops::asin(d.y.clamp(-1.0, 1.0)),
        }
    }

    /// 当前参数下的太阳方向（场景指向太阳）。
    pub(crate) fn sun_dir(&self) -> Vec3 {
        let (sin_az, cos_az) = ops::sin_cos(self.azimuth);
        let (sin_el, cos_el) = ops::sin_cos(self.elevation);
        Vec3::new(sin_az * cos_el, sin_el, cos_az * cos_el)
    }
}

/// 中键拖拽的一帧增量（屏幕系穹顶拖拽）：太阳方向绕相机右轴/上轴旋转，像
/// 被光标按在穹顶上拖——右拖向屏右、下拖向屏下。轴用相机基：俯视等非常规
/// 机位下太阳跟随光标方向（绕世界 Y 在俯视时会原地打转，天顶附近方位角无感）。
/// 增量回写球坐标并夹仰角限位（贴地平线后继续拖只动方位，语义同旧版）。
fn light_rotate(orbit: &mut SunOrbit, delta: Vec2, turn: f32, cam_right: Vec3, cam_up: Vec3) {
    let d = orbit.sun_dir();
    let d = Quat::from_axis_angle(cam_up, delta.x * turn) * d;
    let d = Quat::from_axis_angle(cam_right, delta.y * turn) * d;
    *orbit = SunOrbit::from_dir(d);
    orbit.elevation = orbit.elevation.clamp(MIN_SUN_ELEVATION, MAX_SUN_ELEVATION);
}

/// 球坐标写回灯光朝向：方向光沿实体 forward 照射，forward 钉在
/// 太阳方向的反向（`look_at`，up = 世界 Y）。BRP `sun_set` 与拖拽共用。
pub fn apply_sun_rotation(orbit: &SunOrbit, transform: &mut Transform) {
    transform.look_at(-orbit.sun_dir(), Vec3::Y);
}

/// 日光控制插件：Update 里读中键拖拽改写方向光朝向，接线收在本插件内，
/// main 只 `add_plugins`。作用对象 = 携带 [`SunOrbit`] 的方向光实体。
pub struct AshLightControlPlugin;

impl Plugin for AshLightControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PointerCapture>()
            .add_systems(Update, light_control.after(PointerCaptureSet));
    }
}

/// 日光方向控制（Update）：中键按下且指针不在 UI 上 → 开拖拽会话，按住期间
/// 逐帧位移换算方位/仰角。写 Transform 只在参数变化时——进场朝向（与官方
/// 对照进程同源）在首次中键拖拽前原样保留。
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
    // 主窗未建/最小化：无指针域，拖拽会话就地作废（等还原后重新按下）。
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

    // 中键拖拽旋转：屏幕位移 → 屏幕系旋转，转速与相机拖拽同款（拖满一屏高 =
    // 转一圈）。旋转轴取相机基（无相机退化为世界 Y 系——语义同旧版）；相机
    // 基是上一帧传播结果，滞后一帧对"跟随光标"的手感无感。
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

    // 首次接管报一次即歇（控制链路活了的收账，参数随拖拽持续变化）。
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

    /// 平视相机（右=+X、上=+Y）下拖右：太阳向屏右（+X）转、仰角不变——
    /// 与旧版世界 Y 方位角在水平机位下的行为一致。
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

    /// 俯视相机（右=+X、上=−Z）右拖：天顶附近的太阳应向屏右（+X）移——
    /// 旧版绕世界 Y 在此机位下太阳原地打转（天顶是 Y 轴旋转不动点）。
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
