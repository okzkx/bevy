//! BRP 虚拟鼠标（3.13）：Remote 操作者的窗口无关鼠标输入源——移动/按下/抬起/
//! 滚轮经 BRP handler 注入 [`Messages<WindowEvent>`]，与 bevy_winit 喂引擎的
//! 同一层。下游全部零改动吃到虚拟事件：官方拆分系统 `send_typed_window_events`
//! （PreUpdate `WindowEventSystems`，`.before(InputSystems)`）把事件拆成类型化
//! 消息，其中 `CursorMoved` 分支同步更新 `Window` 内部光标位（bevy_window/src/
//! system.rs:45-62），于是 `Window::cursor_position()`、`ButtonInput<MouseButton>`、
//! [`AccumulatedMouseScroll`]、egui 输入桥（`overlay::input`）与相机轨道控制
//! （`scene::mechanism::camera_control`）各自照常工作。
//!
//! 刻意不走 [`Window::set_cursor_position`]：它在写入光标位的同时还写
//! `cursor_position_request`，bevy_winit 会拿去真实挪动 OS 光标（碰用户真实
//! 输入，违反输入注入纪律）。虚拟鼠标只进引擎事件缓冲，不经 OS 输入队列、
//! 不抢焦点——这与 `tools/inject_mouse.py`（SendInput 真输入，无人模式纪律
//! 管束）的本质分界。
//!
//! 与真实鼠标的关系：同一缓冲合流、后到者赢——语义是"第二只鼠标"，非替换、
//! 不做独占。时序：handler 在 RemoteLast 写消息，次帧 First 换缓冲后 PreUpdate
//! 生效——一帧延迟，与 `set_transform`/`set_camera` 同口径。
//!
//! 坐标口径：points 域逻辑像素、左上原点（与 `Window::cursor_position`/egui
//! 同域），内部按窗口 scale_factor 换算物理像素。窗口外坐标允许（如实转发，
//! 读取侧 `physical_cursor_position` 越界返回 None，应用视作指针离场）；
//! 非有限值直接拒绝，防毒化状态。

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
    remote::{error_codes, BrpError, BrpResult},
    window::{PrimaryWindow, RawCursorMoved, Window, WindowEvent},
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::remote::invalid_params;

use ash_macros::system;

/// 主窗不存在/被关掉的统一错误（与 `set_camera` 的 NoCamera 同码）。
fn no_primary_window() -> BrpError {
    BrpError {
        code: error_codes::COMPONENT_ERROR,
        message: "World 里没有主窗口（PrimaryWindow）".to_owned(),
        data: None,
    }
}

/// 主窗实体与 scale factor：换算与事件装配共用。宿主单窗，取 PrimaryWindow。
/// `query_filtered` 建查询状态需要 `&mut World`（`world.query` 同款形状）。
fn primary_window(world: &mut World) -> Result<(Entity, f32), BrpError> {
    let mut query = world.query_filtered::<(&Window, Entity), With<PrimaryWindow>>();
    let Ok((window, entity)) = query.single(world) else {
        return Err(no_primary_window());
    };
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
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 x/y（points 域逻辑像素）"))?;
    let p: MouseMoveParams = serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    if !p.x.is_finite() || !p.y.is_finite() {
        return Err(invalid_params("x/y 必须是有限数"));
    }
    let (window, scale) = primary_window(world)?;
    let physical_position = DVec2::new(p.x as f64 * scale as f64, p.y as f64 * scale as f64);
    world
        .resource_mut::<Messages<WindowEvent>>()
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
/// 操作序列先 `mouse_move` 再 press。`all` 释放按"当前按住态"结账，与 press
/// 同帧连发时（按压尚未落账）释放不到，需隔一帧。
#[system]
pub(crate) fn mouse_button(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| {
        invalid_params("params 缺失：需要 action（press/release）与 button（或 all=true）")
    })?;
    let p: MouseButtonParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
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
        // 先结账再写消息：ButtonInput 与 Messages 是两个资源，借不可重叠。
        let held: Vec<MouseButton> = world
            .resource::<ButtonInput<MouseButton>>()
            .get_pressed()
            .copied()
            .collect();
        let (window, _) = primary_window(world)?;
        let mut messages = world.resource_mut::<Messages<WindowEvent>>();
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
    let Some(name) = p.button.as_deref() else {
        return Err(invalid_params("需要 button（left/right/middle/back/forward）或 all=true"));
    };
    let button = named_button(name)
        .ok_or_else(|| invalid_params("未知按钮名：left/right/middle/back/forward 五选一"))?;
    let (window, _) = primary_window(world)?;
    world
        .resource_mut::<Messages<WindowEvent>>()
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
    let value = params.ok_or_else(|| invalid_params("params 缺失：lines 与 pixels 二选一"))?;
    let p: MouseWheelParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    match (p.lines, p.pixels) {
        (Some(lines), None) if lines.is_finite() => {
            let (window, _) = primary_window(world)?;
            world
                .resource_mut::<Messages<WindowEvent>>()
                .write(WindowEvent::MouseWheel(MouseWheel {
                    unit: MouseScrollUnit::Line,
                    x: 0.0,
                    y: lines,
                    window,
                    phase: TouchPhase::Moved,
                }));
            Ok(json!({ "window": window.to_string(), "unit": "line", "y": lines }))
        }
        (None, Some(pixels)) if pixels.is_finite() => {
            let (window, _) = primary_window(world)?;
            world
                .resource_mut::<Messages<WindowEvent>>()
                .write(WindowEvent::MouseWheel(MouseWheel {
                    unit: MouseScrollUnit::Pixel,
                    x: 0.0,
                    y: pixels,
                    window,
                    phase: TouchPhase::Moved,
                }));
            Ok(json!({ "window": window.to_string(), "unit": "pixel", "y": pixels }))
        }
        _ => Err(invalid_params("lines 与 pixels 恰好其一（有限数）")),
    }
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
    let Ok(window) = windows.single() else {
        return Err(no_primary_window());
    };
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
