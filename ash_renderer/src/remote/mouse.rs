//! BRP 虚拟鼠标：窗口无关的鼠标输入源——移动/按下/抬起/滚轮经 handler 注入
//! [`Messages<WindowEvent>`]，与 bevy_winit 喂引擎的同一层。官方
//! `send_typed_window_events`（PreUpdate，`.before(InputSystems)`）把事件拆成类型化
//! 消息并同步 `Window` 内部光标位，于是 `Window::cursor_position()`、
//! `ButtonInput<MouseButton>`、[`AccumulatedMouseScroll`]、egui 输入桥与相机轨道
//! 控制都零改动吃到虚拟事件。
//!
//! 坑：不能用 [`Window::set_cursor_position`]——它同时写 `cursor_position_request`，
//! bevy_winit 会据此真实挪动 OS 光标（碰用户真实输入）。虚拟鼠标只进引擎事件
//! 缓冲，不经 OS 输入队列、不抢焦点，这是与 `tools/inject_mouse.py`（SendInput
//! 真输入，仅无人模式可用）的本质分界。
//!
//! 与真实鼠标同一缓冲合流、后到者赢（"第二只鼠标"，不独占）。时序：handler 在
//! RemoteLast 写消息，次帧 PreUpdate 生效，延迟一帧。坐标为 points 域逻辑像素、
//! 左上原点（与 `Window::cursor_position`/egui 同域），内部按 scale_factor 换算
//! 物理像素；窗口外坐标如实转发（读取侧越界返回 None，视作指针离场），非有限值
//! 直接拒绝。

use bevy::{
    input::{
        mouse::{
            AccumulatedMouseScroll, MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel,
        },
        touch::TouchPhase,
        ButtonInput, ButtonState,
    },
    math::DVec2,
    prelude::*,
    remote::{BrpError, BrpResult},
    window::{PrimaryWindow, RawCursorMoved, Window, WindowEvent},
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::params::{invalid_params, parse_params, res, res_mut};

use ash_macros::system;

/// 主窗不存在/被关掉的统一错误（与 `set_camera` 的无相机错误同码）。
fn no_primary_window() -> BrpError {
    BrpError::component_error("World 里没有主窗口（PrimaryWindow）")
}

/// 主窗实体与 scale factor：换算与事件装配共用。宿主单窗，取 PrimaryWindow。
fn primary_window(world: &mut World) -> Result<(Entity, f32), BrpError> {
    let mut query = world.query_filtered::<(&Window, Entity), With<PrimaryWindow>>();
    let (window, entity) = query.single(world).map_err(|_| no_primary_window())?;
    Ok((entity, window.scale_factor()))
}

/// [`MouseButton`] ↔ 协议字符串名的双向映射（BRP 面只暴露五个具名键，
/// `Other` 不收外名，状态读回按 `other` 报）。
fn named_button(name: &str) -> Option<MouseButton> {
    Some(match name {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "back" => MouseButton::Back,
        "forward" => MouseButton::Forward,
        _ => return None,
    })
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        MouseButton::Back => "back",
        MouseButton::Forward => "forward",
        MouseButton::Other(_) => "other",
    }
}

/// `ash_renderer/mouse_move` 的请求参数。
#[derive(Deserialize)]
struct MouseMoveParams {
    x: f32,
    y: f32,
}

/// `ash_renderer/mouse_move`：虚拟指针移动到 (x, y)（points 域逻辑像素）。
/// 经 `WindowEvent::CursorMoved` 走官方拆分通路——`Window` 光标位、egui 指针
/// 与相机拖拽基准随之更新。绝对定位语义（幂等，非增量）：增量拖拽由相机侧
/// 逐帧差分（`DragSession`），操作者只需按轨迹逐点发绝对位置。
#[system]
pub(crate) fn mouse_move(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: MouseMoveParams = parse_params(params, "params 缺失：需要 x/y（points 域逻辑像素）")?;
    if !p.x.is_finite() || !p.y.is_finite() {
        return Err(invalid_params("x/y 必须是有限数"));
    }
    let (window, scale) = primary_window(world)?;
    let physical_position = DVec2::new(p.x as f64 * scale as f64, p.y as f64 * scale as f64);
    res_mut::<Messages<WindowEvent>>(world)?
        .write(WindowEvent::CursorMoved(RawCursorMoved {
            window,
            physical_position,
        }));
    Ok(json!({
        "window": window.to_string(),
        "position": [p.x, p.y],
        "note": "下一帧生效（RemoteLast 写入，PreUpdate 拆分）",
    }))
}

/// `ash_renderer/mouse_button` 的请求参数：单键 press/release，或
/// `{"action": "release", "all": true}` 释放全部按住键（按下后连接断掉的保险）。
#[derive(Deserialize)]
struct MouseButtonParams {
    action: String,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    all: bool,
}

/// `ash_renderer/mouse_button`：虚拟按键按下/抬起 → `WindowEvent::MouseButtonInput`。
/// 注意 egui 桥与相机轨道都以"指针在场"为按压前提（无位置按压无落点）——
/// 操作序列先 `mouse_move` 再 press。`all` 按当前按住态释放，与 press 同帧
/// 连发时（按压尚未生效）释放不到，需隔一帧。
#[system]
pub(crate) fn mouse_button(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: MouseButtonParams = parse_params(
        params,
        "params 缺失：需要 action（press/release）与 button（或 all=true）",
    )?;
    let state = match p.action.as_str() {
        "press" => ButtonState::Pressed,
        "release" => ButtonState::Released,
        _ => return Err(invalid_params("action 须为 press / release")),
    };
    if p.all {
        if p.button.is_some() {
            return Err(invalid_params("all 与 button 互斥"));
        }
        if state == ButtonState::Pressed {
            return Err(invalid_params("all 仅支持 action=release"));
        }
        // 先收集按住键再写消息：ButtonInput 与 Messages 是两个资源，借用不可重叠
        let held: Vec<MouseButton> = res::<ButtonInput<MouseButton>>(world)?
            .get_pressed()
            .copied()
            .collect();
        let (window, _) = primary_window(world)?;
        let mut messages = res_mut::<Messages<WindowEvent>>(world)?;
        for button in &held {
            messages.write(WindowEvent::MouseButtonInput(MouseButtonInput {
                window,
                button: *button,
                state,
            }));
        }
        return Ok(json!({
            "window": window.to_string(),
            "action": "release",
            "all": true,
            "released": held.iter().map(|b| button_name(*b)).collect::<Vec<_>>(),
        }));
    }
    let name = p
        .button
        .as_deref()
        .ok_or_else(|| invalid_params("需要 button（left/right/middle/back/forward）或 all=true"))?;
    let button = named_button(name)
        .ok_or_else(|| invalid_params("未知按钮名：left/right/middle/back/forward 五选一"))?;
    let (window, _) = primary_window(world)?;
    res_mut::<Messages<WindowEvent>>(world)?
        .write(WindowEvent::MouseButtonInput(MouseButtonInput {
            window,
            button,
            state,
        }));
    Ok(json!({
        "window": window.to_string(),
        "button": name,
        "action": p.action,
    }))
}

/// `ash_renderer/mouse_wheel` 的请求参数：lines（格）与 pixels（像素单位）二选一。
#[derive(Deserialize)]
struct MouseWheelParams {
    #[serde(default)]
    lines: Option<f32>,
    #[serde(default)]
    pixels: Option<f32>,
}

/// `ash_renderer/mouse_wheel`：虚拟滚轮 → `WindowEvent::MouseWheel`。只做垂直
/// 滚动（x=0）：相机推拉与调试面板滚动都是纵向；正值 = 向上滚 = 推近
///（相机侧 `radius × 0.95^lines`）。lines/pixels 恰好其一。
#[system]
pub(crate) fn mouse_wheel(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: MouseWheelParams = parse_params(params, "params 缺失：lines 与 pixels 二选一")?;
    let (unit, unit_name, y) = match (p.lines, p.pixels) {
        (Some(lines), None) if lines.is_finite() => (MouseScrollUnit::Line, "line", lines),
        (None, Some(pixels)) if pixels.is_finite() => (MouseScrollUnit::Pixel, "pixel", pixels),
        _ => return Err(invalid_params("lines 与 pixels 恰好其一（有限数）")),
    };
    let (window, _) = primary_window(world)?;
    res_mut::<Messages<WindowEvent>>(world)?
        .write(WindowEvent::MouseWheel(MouseWheel {
            unit,
            x: 0.0,
            y,
            window,
            phase: TouchPhase::Moved,
        }));
    Ok(json!({ "window": window.to_string(), "unit": unit_name, "y": y }))
}

/// `ash_renderer/mouse_status`：虚拟/真实鼠标当前状态读回（操作者反馈环）——
/// 光标位置（points 域，None = 离窗/越界）、按住键清单、窗口尺寸与
/// scale_factor、本帧累计滚轮（RemoteLast 时点 = Update 消费到的值）。
#[system]
pub(crate) fn mouse_status(
    In(_params): In<Option<Value>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    buttons: Res<ButtonInput<MouseButton>>,
    scroll: Res<AccumulatedMouseScroll>,
) -> BrpResult {
    let window = windows.single().map_err(|_| no_primary_window())?;
    let held: Vec<&str> = buttons.get_pressed().map(|b| button_name(*b)).collect();
    Ok(json!({
        "position": window.cursor_position().map(|p| [p.x, p.y]),
        "held": held,
        "window": {
            "logical": [window.width(), window.height()],
            "physical": [window.physical_width(), window.physical_height()],
            "scale_factor": window.scale_factor(),
        },
        "scroll": {
            "unit": match scroll.unit {
                MouseScrollUnit::Line => "line",
                MouseScrollUnit::Pixel => "pixel",
            },
            "delta": [scroll.delta.x, scroll.delta.y],
        },
    }))
}
