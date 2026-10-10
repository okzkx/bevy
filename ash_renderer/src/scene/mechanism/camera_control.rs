//! 相机轨道控制（机制半边）：左键拖动绕目标点环绕、滚轮沿视线推拉缩放，
//! 零 Vulkan 代码，也不含写死的场景参数——环绕目标由组件携带，进场时由
//! 业务半边（[`crate::scene::content::camera`]）按取景参数给出。
//! 输入源有两个：鼠标（左键 + 滚轮，逐帧增量）与 CLI 给值（3.12 BRP
//! `set_camera`，[`apply_camera_command`] 绝对值/系数覆盖，共用同一套球坐标
//! 状态与 Transform 写路径）。
//!
//! 取舍：官方 0.21 新增的 `bevy_camera_controller::pan_orbit_camera` 是完整版
//!（bevy_picking 命中测试决定旋转中心、动量/平滑可调、bevy_render 不可选依赖），
//! 禁渲染宿主挂不上也用不着——我们只要"固定中心点的检视轨道"这一档，球坐标
//! 参数化（yaw/pitch/radius）自写，语义取 three.js OrbitControls 同款：拖动按
//! 窗口高度归一（拖满一屏高 = 转一整圈）、滚轮每格 ×0.95 指数缩放。
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
        mouse::{AccumulatedMouseScroll, MouseButton, MouseScrollPixelsPerLine},
        ButtonInput,
    },
    math::ops,
    prelude::*,
    window::{PrimaryWindow, Window},
};

use crate::overlay::EguiState;

use ash_macros::system;

/// 环绕转速：拖满一个窗口高度 = 转一整圈（three.js OrbitControls 的
/// `2π·Δ/height` 同款归一，窗口 resize 后手感不漂移；位移用 points 域，
/// DPI 无关）。
const FULL_TURN_PER_HEIGHT: f32 = std::f32::consts::TAU;

/// 每格滚轮的缩放系数：滚近 ×0.95、滚远 ×1/0.95（three.js 默认 zoomSpeed 同款），
/// 指数缩放保证远近手感一致。
const ZOOM_PER_LINE: f32 = 0.95;

/// 环绕半径限位：下限防穿模（头盔半身尺 ~0.3，0.5 时贴脸近裁剪面也安全），
/// 上限防把模型缩成一点后找不到。
const MIN_ORBIT_RADIUS: f32 = 0.5;
const MAX_ORBIT_RADIUS: f32 = 10.0;

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

/// CLI 相机给值命令（3.12）：逐项可选（None = 不动该量）。角度用度数（与
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

/// 相机命令被拒的原因：World 里没有带 [`CameraOrbit`] 的相机。
#[derive(Debug)]
pub enum CameraCommandError {
    NoCamera,
}

/// 应用一次 CLI 相机给值（3.12）：3.9 的鼠标输入与 3.12 的 BRP 输入汇入同一
/// 套球坐标状态——限位常量、target+offset 加 look_at 的 Transform 写路径全部
/// 复用，无两套机位。命令在 RemoteLast（帧尾）执行，写值下一帧渲染。
/// 返回命令后的轨道参数（读回值，供 BRP 响应回显）。
pub fn apply_camera_command(
    world: &mut World,
    cmd: CameraOrbitCommand,
) -> Result<CameraOrbit, CameraCommandError> {
    let mut query = world.query::<(&mut Transform, &mut CameraOrbit, &Camera)>();
    let Ok((mut transform, mut orbit, _)) = query.single_mut(world) else {
        return Err(CameraCommandError::NoCamera);
    };
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

/// 相机轨道控制插件：Update 里读鼠标输入改写相机位姿，接线收在本插件内，
/// main 只 `add_plugins`。作用对象 = 带 [`CameraOrbit`] 的相机。
pub struct AshCameraControlPlugin;

impl Plugin for AshCameraControlPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, orbit_camera_control);
    }
}

/// 轨道控制（Update）：左键按下且 egui 不感兴趣 → 开启拖拽会话，按住期间
/// 逐帧位移换算 yaw/pitch；滚轮按格缩放半径。写 Transform 只在参数变化时，
/// 朝向恒用 look_at 对准环绕中心（与业务半边进场的 looking_at 同语义，
/// 相机就位核验的 0° 判定线在拖拽后仍成立）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单：窗口/鼠标/滚轮/换算率/egui 仲裁/相机组/拖拽会话/首接管收账，逐项声明是框架惯例"
)]
#[system]
fn orbit_camera_control(
    windows: Query<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    scroll: Res<AccumulatedMouseScroll>,
    pixels_per_line: Res<MouseScrollPixelsPerLine>,
    egui: Option<Res<EguiState>>,
    mut cameras: Query<(&mut Transform, &mut CameraOrbit), With<Camera>>,
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

    let mut dirty = false;
    // 滚轮推拉（3.9.2）：悬在 egui 区上滚轮让给调试窗口（该帧半径不动，扫出
    // 后恢复）；拖拽中指针扫过面板同理，推拉与环绕正交。
    let lines = scroll.to_lines(&pixels_per_line).delta.y;
    if lines != 0.0 && !egui_wants {
        for (_, mut orbit) in &mut cameras {
            orbit.radius = (orbit.radius * ops::powf(ZOOM_PER_LINE, lines))
                .clamp(MIN_ORBIT_RADIUS, MAX_ORBIT_RADIUS);
            dirty = true;
        }
    }
    // 左键拖拽环绕（3.9.1）：屏幕位移 → 球角。方向取"拖动世界"约定
    //（three.js 同款）：右拖 = 相机绕左、物体呈顺时针转；下拖 = 相机升、露头顶。
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
            "相机控制首次接管：yaw {:.3} rad / pitch {:.3} rad / radius {:.3}（左键拖拽环绕 + 滚轮推拉已生效）",
            orbit.yaw, orbit.pitch, orbit.radius,
        );
    }
}
