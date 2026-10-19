//! 相机控制（机制半边）：左键拖动绕目标点环绕、滚轮沿视线推拉缩放；5.4 起
//! 追加 Unity Scene 式漫游——右键拖拽环视 + WASD 沿视线平移 + Q/E 世界升降。
//! 零 Vulkan 代码，也不含写死的场景参数——环绕目标由组件携带，进场时由
//! 业务半边（[`crate::scene::content::camera`]）按取景参数给出。
//! 输入源有两个：鼠标（左键 + 滚轮，逐帧增量）与 CLI 给值（BRP
//! `set_camera`，[`apply_camera_command`] 绝对值/系数覆盖，共用同一套球坐标
//! 状态与 Transform 写路径）。
//!
//! 取舍：官方 0.21 新增的 `bevy_camera_controller::pan_orbit_camera` 是完整版
//!（bevy_picking 命中测试决定旋转中心、动量/平滑可调、bevy_render 不可选依赖），
//! 禁渲染宿主挂不上也用不着——我们只要"固定中心点的检视轨道"这一档，球坐标
//! 参数化（yaw/pitch/radius）自写：拖动按
//! 窗口高度归一（拖满一屏高 = 转一整圈）、滚轮每格 ×0.95 指数缩放。
//!
//! 漫游与轨道共用一套球坐标状态（单一机位真源，BRP 给值照常生效）：右键
//! 环视 = 相机位置钉住、yaw/pitch 随拖拽旋转、环绕中心搬到新视线正前方
//!（pivot 跟随，松开右键后左键轨道绕新中心继续——Unity Scene 同款）；
//! WASD 沿相机视线系平移（W = 视线方向），Q/E 世界升降；速度 = radius·系数
//! 每秒（缩放档位即速度档位，头盔近景与大场景俯瞰同一手感）。键盘侧仲裁 =
//! egui `wants_keyboard_input()`（面板文本编辑中让键），鼠标侧同 layer_id_at
//! 门。局限：环视不做指针抓取，光标拖出窗口即无增量（会话保活、回窗续拖，
//! 与左键会话同款防跳变）。
//!
//! 帧内时序：Update 改 [`Transform`] → PostUpdate 传播（TransformSystems::Propagate）
//! → Last 的 draw_frame 直读 `GlobalTransform` 建 VP（driver/host/frame.rs
//! `FrameInput.cameras`）——同帧生效，无跨帧滞后。
//!
//! 与 egui 调试 UI 的仲裁：按下/滚轮时刻查指针位置上的最顶层
//!（`layer_id_at`，egui 0.36.2 context.rs:3104）——非 Background 层 = 悬在
//! 调试面板上，就让给 UI。不用 `is_pointer_over_egui()`：它依赖 `run_ui`
//! 闭包 API 写入的 `root_ui_available_rect`，本集成的 `begin_pass`/`end_pass`
//! 形状下该字段恒 None，指针在背景上时恒返回 true（实测门永远关死）。
//! 光标 points 域与 egui 同域直比。查询读的是最近一次 pass 的层序，最多
//! 滞后一帧——点击瞬间指针不会瞬移，调试 UI 场景可接受。

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

use crate::overlay::EguiState;

use ash_macros::system;

/// 环绕转速：拖满一个窗口高度 = 转一整圈（`2π·Δ/height` 归一，窗口 resize
/// 后手感不漂移；位移用 points 域，DPI 无关）。
const FULL_TURN_PER_HEIGHT: f32 = std::f32::consts::TAU;

/// 每格滚轮的缩放系数：滚近 ×0.95、滚远 ×1/0.95，
/// 指数缩放保证远近手感一致。
const ZOOM_PER_LINE: f32 = 0.95;

/// 漫游速度系数：每秒平移 radius·系数。缩放档位即速度档位——radius 300
/// 的俯瞰档跑得快、radius 3 的检视档走得慢，同一手感覆盖头盔与大场景。
const FLY_SPEED_FACTOR: f32 = 0.5;

/// 环绕半径限位：下限防穿模（头盔半身尺 ~0.3，0.5 时贴脸近裁剪面也安全）；
/// 上限原为 FlightHelmet 小场景钉 10.0，5.4 起常驻大场景（4096 实例铺
/// ~256 单位）取景需要俯瞰全程，放宽到 2000（5.5 三档压测同够用——远平面
/// 同步放宽，见 content/camera）。
const MIN_ORBIT_RADIUS: f32 = 0.5;
const MAX_ORBIT_RADIUS: f32 = 2000.0;

/// 俯仰限位：距极点留 1° 余量，`offset()` 里 `cos(pitch)` 恒非零，
/// look_at 的 up=Y 才不退化。
const PITCH_LIMIT: f32 = 89.0f32.to_radians();

/// 轨道控制状态：球坐标（yaw 绕 Y、pitch 仰角）+ 半径 + 环绕目标。
/// 目标点属于场景内容（看哪件东西），由业务半边在 spawn 时按取景参数写入。
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
    /// 从现役机位反解轨道参数：位置/注视点已有的相机（如业务半边 spawn 的
    /// 初始机位）零跳变接管。
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
#[derive(Default)]
struct DragSession {
    dragging: bool,
    /// None = 指针离窗/未入场：不产生增量，重入时重新取基准（防跳变）。
    last_cursor: Option<Vec2>,
}

/// CLI 相机给值命令：逐项可选（None = 不动该量）。角度用度数（与
/// [`crate::overlay::transform_edit::TransformEdit`] 的欧拉口径一致），zoom
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

/// 单相机命中后的命令落盘（原内联体摘出，多/无相机分流在上层）。
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

/// 相机控制插件：Update 里读鼠标/键盘输入改写相机位姿，接线收在本插件内，
/// main 只 `add_plugins`。作用对象 = 带 [`CameraOrbit`] 的相机。
pub struct AshCameraControlPlugin;

impl Plugin for AshCameraControlPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, camera_control);
    }
}

/// 相机控制（Update）：左键按下且 egui 不感兴趣 → 开启轨道拖拽会话，按住
/// 期间逐帧位移换算 yaw/pitch；右键同款会话做环视（位置钉住、pivot 跟随）；
/// 滚轮按格缩放半径；WASDQE 漫游平移。写 Transform 只在参数变化时，朝向恒
/// 用 look_at 对准环绕中心（与业务半边进场的 looking_at 同语义，相机就位
/// 核验的 0° 判定线在拖拽后仍成立）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：窗口/鼠标/键盘/滚轮/换算率/时间/egui 仲裁/相机组/双拖拽会话/首接管收账，逐项声明是框架惯例"
)]
#[system]
fn camera_control(
    windows: Query<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    scroll: Res<AccumulatedMouseScroll>,
    pixels_per_line: Res<MouseScrollPixelsPerLine>,
    time: Res<Time>,
    egui: Option<Res<EguiState>>,
    mut cameras: Query<(&mut Transform, &mut CameraOrbit), With<Camera>>,
    mut session: Local<DragSession>,
    mut look: Local<DragSession>,
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
    // egui 仲裁（语义见模块文档）：光标 points 域与 egui 同域，直接问"这点
    // 上最顶层是谁"，非 Background 层 = 悬在调试面板上。
    let egui_wants = match (egui.as_deref(), cursor) {
        (Some(state), Some(pos)) => state
            .ctx
            .layer_id_at(egui::pos2(pos.x, pos.y))
            .is_some_and(|layer| layer.order != egui::Order::Background),
        _ => false,
    };

    if mouse.just_pressed(MouseButton::Left) && !egui_wants {
        session.dragging = cursor.is_some();
        session.last_cursor = cursor;
    }
    if mouse.just_released(MouseButton::Left) {
        session.dragging = false;
        session.last_cursor = None;
    }
    if mouse.just_pressed(MouseButton::Right) && !egui_wants {
        look.dragging = cursor.is_some();
        look.last_cursor = cursor;
    }
    if mouse.just_released(MouseButton::Right) {
        look.dragging = false;
        look.last_cursor = None;
    }

    let mut dirty = false;
    // 滚轮推拉：悬在 egui 区上滚轮让给调试窗口（该帧半径不动，扫出
    // 后恢复）；拖拽中指针扫过面板同理，推拉与环绕正交。
    let lines = scroll.to_lines(&pixels_per_line).delta.y;
    if lines != 0.0 && !egui_wants {
        for (_, mut orbit) in &mut cameras {
            orbit.radius = (orbit.radius * ops::powf(ZOOM_PER_LINE, lines))
                .clamp(MIN_ORBIT_RADIUS, MAX_ORBIT_RADIUS);
            dirty = true;
        }
    }
    // 左键拖拽环绕：屏幕位移 → 球角。方向取"拖动世界"约定：右拖 = 相机绕左、物体呈顺时针转；下拖 = 相机升、露头顶。
    if session.dragging
        && mouse.pressed(MouseButton::Left)
        && let Some(pos) = cursor
    {
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
    // 右键拖拽环视：位置钉住、yaw/pitch 旋转、pivot 搬到新视线正前方
    //（[`look_rotate`]，Unity Scene 同款）；egui 门与左键同款。
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
    let egui_wants_kb = egui.as_deref().is_some_and(|s| s.ctx.egui_wants_keyboard_input());
    if !egui_wants_kb {
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
    // 首次接管报一次即歇（控制链路活了的收账，参数随拖拽/滚轮持续变化）。
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

    /// 鼠标下拖 = 视线下俯（Unity Scene 同款，与左键"下拖相机升"同号）。
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
