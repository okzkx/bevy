//! 相机输入控制：左键轨道、右键环视、滚轮缩放、WASDQE 漫游；BRP 命令共用球坐标状态。
//! Update 写 Transform，传播后由 Last 的帧循环读取；系统排在 PointerCaptureSet 之后。
//! 不抓取指针；UI 仲裁由 PointerCapture 提供。右键拖动未检查 UI 悬停状态。

use bevy::{
    camera::Camera,
    input::{
        keyboard::KeyCode,
        mouse::{AccumulatedMouseScroll, MouseButton, MouseScrollPixelsPerLine},
        ButtonInput,
    },
    math::ops,
    prelude::*,
    window::{PrimaryWindow, Window},
};

use super::pointer_capture::{PointerCapture, PointerCaptureSet};

use ash_macros::system;

/// 拖满一个窗口高度转一周；位移使用 points 域，与 DPI 无关。相机和日光控制共用。
pub(crate) const FULL_TURN_PER_HEIGHT: f32 = std::f32::consts::TAU;

/// 每格滚轮的缩放系数：滚近 ×0.95、滚远 ×1/0.95，
/// 指数缩放保证远近手感一致。
const ZOOM_PER_LINE: f32 = 0.95;

/// 每秒平移速度为 radius × 系数，缩放半径同时改变漫游速度。
const FLY_SPEED_FACTOR: f32 = 0.5;

/// 环绕距离限制；上限应与场景相机的远平面设置匹配。
const MIN_ORBIT_RADIUS: f32 = 0.5;
const MAX_ORBIT_RADIUS: f32 = 2000.0;

/// 俯仰限位：距极点留 1° 余量，`offset()` 里 `cos(pitch)` 恒非零，
/// look_at 的 up=Y 才不退化。
const PITCH_LIMIT: f32 = 89.0f32.to_radians();

/// 轨道控制状态；目标点由场景内容在 spawn 时提供。
#[derive(Component, Debug, Clone, Copy)]
pub struct CameraOrbit {
    /// 环绕中心（世界系）。
    pub target: Vec3,
    /// 方位角（rad）：绕 +Y，0 = 相机在 +Z 侧（x = r·sin(yaw)·cos(pitch)）。
    pub yaw: f32,
    /// 仰角（rad）：±PITCH_LIMIT，0 = 相机在水平面上。
    pub pitch: f32,
    /// 到中心距离。
    pub radius: f32,
}

impl CameraOrbit {
/// 从现有相机位置和注视点反解轨道参数。
    pub fn from_pose(position: Vec3, target: Vec3) -> Self {
        let offset = position - target;
        Self {
            target,
            yaw: ops::atan2(offset.x, offset.z),
            pitch: ops::atan2(offset.y, offset.xz().length())
                .clamp(-PITCH_LIMIT, PITCH_LIMIT),
            radius: offset.length().max(MIN_ORBIT_RADIUS),
        }
    }

    /// 当前参数下的相机偏移（位置 = target + offset）。
    fn offset(&self) -> Vec3 {
        let (sin_yaw, cos_yaw) = ops::sin_cos(self.yaw);
        let (sin_pitch, cos_pitch) = ops::sin_cos(self.pitch);
        self.radius * Vec3::new(sin_yaw * cos_pitch, sin_pitch, cos_yaw * cos_pitch)
    }
}

/// 一次拖拽的跨帧会话：接管标志 + 上一帧指针位置（points 域）。
/// 机制半边的拖拽控制（相机 / 日光）共用。
#[derive(Default)]
pub(crate) struct DragSession {
    pub(crate) dragging: bool,
    /// None 表示没有记录的上一帧光标位置。
    pub(crate) last_cursor: Option<Vec2>,
}

/// CLI 相机给值命令：逐项可选（None = 不动该量）。角度用度数（与
/// [`TransformEdit`] 的欧拉口径一致），zoom
/// 与滚轮的指数缩放同语义（<1 推近、>1 拉远）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CameraOrbitCommand {
    /// 环绕中心（世界系）。
    pub target: Option<Vec3>,
    /// 方位角（度，绝对值覆盖）。
    pub yaw_deg: Option<f32>,
    /// 仰角（度，绝对值覆盖，限位同拖拽）。
    pub pitch_deg: Option<f32>,
    /// 距中心距离（绝对值覆盖，限位同滚轮）。
    pub radius: Option<f32>,
    /// 缩放系数：radius *= zoom 后限位（0.8 = 推近 20%）。
    pub zoom: Option<f32>,
}

/// 相机命令被拒的原因（多相机与无相机分开报，机位目标不明确与无机位可区分）。
#[derive(Debug)]
pub enum CameraCommandError {
    /// World 里没有带 [`CameraOrbit`] 的相机。
    NoCamera,
    /// 带相机实体不止一台（single_mut 拒绝），机位目标不明确。
    AmbiguousCamera,
}

/// 应用一次 CLI 相机给值：鼠标输入与 BRP 输入汇入同一
/// 套球坐标状态——限位常量、target+offset 加 look_at 的 Transform 写路径全部
/// 复用，无两套机位。命令在 RemoteLast（帧尾）执行，写值下一帧渲染。
/// 返回命令后的轨道参数（读回值，供 BRP 响应回显）。
pub fn apply_camera_command(
    world: &mut World,
    cmd: CameraOrbitCommand,
) -> Result<CameraOrbit, CameraCommandError> {
    let mut query = world.query::<(&mut Transform, &mut CameraOrbit, &Camera)>();
    match query.single_mut(world) {
        Ok((mut transform, mut orbit, _)) => {
            apply_orbit_command(&mut transform, &mut orbit, &cmd)
        }
        Err(bevy::ecs::query::QuerySingleError::MultipleEntities(_)) => {
            Err(CameraCommandError::AmbiguousCamera)
        }
        Err(_) => Err(CameraCommandError::NoCamera),
    }
}

/// 应用命令到已选中的相机。
fn apply_orbit_command(
    transform: &mut Transform,
    orbit: &mut CameraOrbit,
    cmd: &CameraOrbitCommand,
) -> Result<CameraOrbit, CameraCommandError> {
    if let Some(target) = cmd.target {
        orbit.target = target;
    }
    if let Some(yaw) = cmd.yaw_deg {
        orbit.yaw = yaw.to_radians();
    }
    if let Some(pitch) = cmd.pitch_deg {
        orbit.pitch = pitch.to_radians().clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }
    if let Some(radius) = cmd.radius {
        orbit.radius = radius.clamp(MIN_ORBIT_RADIUS, MAX_ORBIT_RADIUS);
    }
    if let Some(zoom) = cmd.zoom {
        orbit.radius = (orbit.radius * zoom).clamp(MIN_ORBIT_RADIUS, MAX_ORBIT_RADIUS);
    }
    transform.translation = orbit.target + orbit.offset();
    transform.look_at(orbit.target, Vec3::Y);
    Ok(*orbit)
}

/// 右键环视的一帧增量：相机位置钉住不动，yaw/pitch 随拖拽换算（方向与左键
/// 同号——右拖 = 视线右转、下拖 = 视线下俯），环绕中心搬到新视线正前方等距
/// 处（pivot 跟随，radius 不变）。
fn look_rotate(orbit: &mut CameraOrbit, delta: Vec2, turn: f32) {
    let position = orbit.target + orbit.offset();
    orbit.yaw -= delta.x * turn;
    orbit.pitch = (orbit.pitch + delta.y * turn).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    orbit.target = position - orbit.offset();
}

/// 漫游平移向量（未归一）：WASD 沿视线系（forward = 视线方向），Q/E 世界
/// 升降。俯仰限位保证 forward 不与 Y 平行，right 不退化。
fn fly_vector(forward: Vec3, keys: &ButtonInput<KeyCode>) -> Vec3 {
    let right = forward.cross(Vec3::Y);
    let mut v = Vec3::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        v += forward;
    }
    if keys.pressed(KeyCode::KeyS) {
        v -= forward;
    }
    if keys.pressed(KeyCode::KeyD) {
        v += right;
    }
    if keys.pressed(KeyCode::KeyA) {
        v -= right;
    }
    if keys.pressed(KeyCode::KeyE) {
        v += Vec3::Y;
    }
    if keys.pressed(KeyCode::KeyQ) {
        v -= Vec3::Y;
    }
    v
}

/// 注册相机控制系统及指针仲裁资源。
pub struct AshCameraControlPlugin;

impl Plugin for AshCameraControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PointerCapture>()
            .add_systems(Update, camera_control.after(PointerCaptureSet));
    }
}

/// Update 中处理轨道、环视、滚轮缩放和漫游输入，并将相机朝向对准环绕中心。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：窗口/鼠标/键盘/滚轮/换算率/时间/UI 仲裁/相机组/双拖拽会话/首接管收账，逐项声明是框架惯例"
)]
#[system]
fn camera_control(
    windows: Query<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    scroll: Res<AccumulatedMouseScroll>,
    pixels_per_line: Res<MouseScrollPixelsPerLine>,
    time: Res<Time>,
    capture: Res<PointerCapture>,
    mut cameras: Query<(&mut Transform, &mut CameraOrbit), With<Camera>>,
    mut session: Local<DragSession>,
    mut look: Local<DragSession>,
    mut engaged: Local<bool>,
) {
    // 主窗口缺席或最小化时，取消左键拖拽会话。
    let Ok(window) = windows.single() else {
        *session = DragSession::default();
        return;
    };
    if window.width() <= 0.0 || window.height() <= 0.0 {
        *session = DragSession::default();
        return;
    }
    let cursor = window.cursor_position();
    // UI 仲裁由 overlay 在本帧发布。
    let ui_wants = capture.over_ui;

    if mouse.just_pressed(MouseButton::Left) && !ui_wants {
        session.dragging = cursor.is_some();
        session.last_cursor = cursor;
    }
    if mouse.just_released(MouseButton::Left) {
        session.dragging = false;
        session.last_cursor = None;
    }
    if mouse.just_pressed(MouseButton::Right) && !ui_wants {
        look.dragging = cursor.is_some();
        look.last_cursor = cursor;
    }
    if mouse.just_released(MouseButton::Right) {
        look.dragging = false;
        look.last_cursor = None;
    }

    let mut dirty = false;
    // 指针位于 UI 上时，滚轮输入留给调试窗口。
    let lines = scroll.to_lines(&pixels_per_line).delta.y;
    if lines != 0.0 && !ui_wants {
        for (_, mut orbit) in &mut cameras {
            orbit.radius = (orbit.radius * ops::powf(ZOOM_PER_LINE, lines))
                .clamp(MIN_ORBIT_RADIUS, MAX_ORBIT_RADIUS);
            dirty = true;
        }
    }
    // 左键拖动屏幕位移转换为方位角和仰角变化。
    if session.dragging
        && mouse.pressed(MouseButton::Left)
        && let Some(pos) = cursor
    {
        if ui_wants {
            session.last_cursor = Some(pos);
        } else {
            if let Some(last) = session.last_cursor {
                let delta = pos - last;
                let turn = FULL_TURN_PER_HEIGHT / window.height();
                for (_, mut orbit) in &mut cameras {
                    orbit.yaw -= delta.x * turn;
                    orbit.pitch = (orbit.pitch + delta.y * turn).clamp(-PITCH_LIMIT, PITCH_LIMIT);
                }
                dirty |= delta != Vec2::ZERO;
            }
            session.last_cursor = Some(pos);
        }
    }
    // 右键拖动旋转视线并移动环绕中心；此路径不检查 UI 悬停状态。
    if look.dragging
        && mouse.pressed(MouseButton::Right)
        && let Some(pos) = cursor
    {
        if let Some(last) = look.last_cursor {
            let delta = pos - last;
            let turn = FULL_TURN_PER_HEIGHT / window.height();
            for (_, mut orbit) in &mut cameras {
                look_rotate(&mut orbit, delta, turn);
            }
            dirty |= delta != Vec2::ZERO;
        }
        look.last_cursor = Some(pos);
    }
    // WASDQE 漫游：WASD 沿视线系、Q/E 世界升降，速度随 radius 自适应；
    // egui 文本编辑中让键（面板输入不被相机劫持）。
    if !capture.wants_keyboard {
        let dt = time.delta_secs();
        for (_, mut orbit) in &mut cameras {
            let v = fly_vector(-orbit.offset().normalize_or_zero(), &keys);
            if v != Vec3::ZERO {
                let step = v.normalize_or_zero() * (orbit.radius * FLY_SPEED_FACTOR * dt);
                orbit.target += step;
                dirty = true;
            }
        }
    }

    if !dirty {
        return;
    }
    for (mut transform, orbit) in &mut cameras {
        transform.translation = orbit.target + orbit.offset();
        transform.look_at(orbit.target, Vec3::Y);
    }
    if !*engaged
        && let Ok((_, orbit)) = cameras.single()
    {
        *engaged = true;
        info!(
            "相机控制首次接管：yaw {:.3} rad / pitch {:.3} rad / radius {:.3}（左键环绕 + 滚轮推拉 + 右键环视 + WASDQE 漫游已生效）",
            orbit.yaw, orbit.pitch, orbit.radius,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::ButtonInput;

    /// 鼠标右拖 = 视线右转，且相机位置钉住不动（环视非环绕）。
    #[test]
    fn look_mouse_right_turns_view_and_keeps_position() {
        let mut orbit =
            CameraOrbit { target: Vec3::ZERO, yaw: 0.0, pitch: 0.0, radius: 4.0 };
        let position = orbit.target + orbit.offset();
        look_rotate(&mut orbit, Vec2::new(150.0, 0.0), 0.01);
        assert!((orbit.target + orbit.offset() - position).length() < 1e-4);
        let forward = (orbit.target - position).normalize();
        assert!(forward.x > 0.9, "右拖后视线应转向 +X，实际 {forward:?}");
    }

    /// 下拖使视线向下俯。
    #[test]
    fn look_mouse_down_pitches_view_down() {
        let mut orbit =
            CameraOrbit { target: Vec3::ZERO, yaw: 0.0, pitch: 0.0, radius: 4.0 };
        look_rotate(&mut orbit, Vec2::new(0.0, 50.0), 0.01);
        let position = orbit.target + orbit.offset();
        let forward = (orbit.target - position).normalize();
        assert!(forward.y < 0.0, "下拖后视线应朝下，实际 {forward:?}");
    }

    /// 漫游轴向：W = 视线前进、D = 视线右侧、E/Q = 世界升/降。
    #[test]
    fn fly_axes_follow_view_and_world_up() {
        let mut keys = ButtonInput::<KeyCode>::default();
        keys.press(KeyCode::KeyW);
        assert_eq!(fly_vector(Vec3::NEG_Z, &keys), Vec3::NEG_Z);
        keys.reset_all();
        keys.press(KeyCode::KeyD);
        assert_eq!(fly_vector(Vec3::NEG_Z, &keys), Vec3::X);
        keys.reset_all();
        keys.press(KeyCode::KeyE);
        assert_eq!(fly_vector(Vec3::NEG_Z, &keys), Vec3::Y);
        keys.reset_all();
        keys.press(KeyCode::KeyQ);
        assert_eq!(fly_vector(Vec3::NEG_Z, &keys), Vec3::NEG_Y);
    }
}
