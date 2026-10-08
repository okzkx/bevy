//! 输入桥：bevy 输入事件 → egui `RawInput`。
//!
//! egui 0.36 的输入面（源码钉死，见施工计划 §2.1）：`RawInput` 没有
//! pixels_per_point 字段（走 `Context::set_pixels_per_point`）、没有顶层 modifiers
//! 字段——修饰键只随 `Key`/`PointerButton`/`MouseWheel` 事件携带，另有独立
//! `Event::ModifiersChanged` 可发状态变化。本桥的做法：每事件附加当前修饰键
//! 快照，修饰键状态变化时补发一条 `ModifiersChanged`。
//!
//! 事件次序只能按类近似（bevy 的各类事件独立缓冲，跨类无全局序）：键盘 →
//! 指针移动/离开 → 按键 → 滚轮。指针先于按键，是让同帧"移动+点击"的按下
//! 事件带上本帧新位置（`MouseButtonInput` 不带坐标，从移动事件补）。
//! 调试 UI 场景够用；完美序要 fork bevy_winit。
//!
//! bevy 0.20 事实：`ReceivedCharacter` 已删，文本随 `KeyboardInput.text` 携带
//!（crates/bevy_input/src/keyboard.rs:134）。文本事件按 egui-winit 同款过滤：
//! 按下态、无 ctrl/command（快捷键不产文本）。
//!
//! 滚轮方向换算照 egui-winit（0.36.2 源码）：Line 单位 x/y 直通、不取反；
//! Pixel 单位除以 pixels_per_point 转 Point 单位。指针位置是逻辑像素
//! （= egui 的 points），无需换算。

use bevy::{
    input::{
        keyboard::{KeyCode, KeyboardInput},
        mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel},
        ButtonInput, ButtonState,
    },
    prelude::*,
    window::{CursorLeft, CursorMoved, Window, WindowFocused},
};

/// 输入桥跨帧状态。
#[derive(Resource, Default)]
pub(crate) struct EguiInput {
    /// 最新指针位置（points）。bevy 的 `MouseButtonInput` 不带位置——发
    /// `PointerButton` 时从这里补 pos；`CursorLeft` 后清空。
    pointer_pos: Option<egui::Pos2>,
    /// 上一帧的修饰键快照：变化时补发 `ModifiersChanged`。
    last_modifiers: egui::Modifiers,
}

/// 组一帧的 egui `RawInput`。事件读取器由调用方（egui pass 系统）持有并传入。
#[expect(
    clippy::too_many_arguments,
    reason = "事件读取器逐项传入是 bevy 系统参数的常态形状，收拢成结构体只会把依赖表换个地方写"
)]
pub(crate) fn egui_raw_input(
    window: &Window,
    time: &Time,
    keys: &ButtonInput<KeyCode>,
    input: &mut EguiInput,
    keyboard: &mut MessageReader<KeyboardInput>,
    mouse_button: &mut MessageReader<MouseButtonInput>,
    wheel: &mut MessageReader<MouseWheel>,
    cursor: &mut MessageReader<CursorMoved>,
    cursor_left: &mut MessageReader<CursorLeft>,
    window_focused: &mut MessageReader<WindowFocused>,
) -> egui::RawInput {
    let modifiers = modifiers_from(keys);
    let mut events = Vec::new();
    for ev in keyboard.read() {
        // 文本事件：按下态、有文本、无 ctrl/command（快捷键不产文本）
        let is_cmd = modifiers.ctrl || modifiers.command || modifiers.mac_cmd;
        if ev.state == ButtonState::Pressed
            && !is_cmd
            && let Some(text) = ev.text.as_ref().filter(|t| !t.is_empty())
        {
            events.push(egui::Event::Text(text.to_string()));
        }
        if let Some(key) = map_key(&ev.key_code) {
            events.push(egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed: ev.state == ButtonState::Pressed,
                repeat: ev.repeat,
                modifiers,
            });
        }
    }
    // 指针移动/离开先于按键处理：bevy 各类事件独立缓冲，同帧可能同时进"移动+按下"，
    // 先更新 pointer_pos 再读按钮，按下事件才带得上本帧的新位置（否则用上一帧的
    // 旧位置、首帧更是 None 整条丢弃）——与 egui-winit 按事件到达序处理同构。
    for ev in cursor.read() {
        let pos = egui::pos2(ev.position.x, ev.position.y);
        input.pointer_pos = Some(pos);
        events.push(egui::Event::PointerMoved(pos));
    }
    for _ in cursor_left.read() {
        input.pointer_pos = None;
        events.push(egui::Event::PointerGone);
    }
    for ev in mouse_button.read() {
        if let Some(pos) = input.pointer_pos {
            events.push(egui::Event::PointerButton {
                pos,
                button: map_button(ev.button),
                pressed: ev.state == ButtonState::Pressed,
                modifiers,
            });
        }
    }
    let ppp = window.scale_factor();
    for ev in wheel.read() {
        let (unit, delta) = match ev.unit {
            MouseScrollUnit::Line => (egui::MouseWheelUnit::Line, egui::vec2(ev.x, ev.y)),
            MouseScrollUnit::Pixel => {
                (egui::MouseWheelUnit::Point, egui::vec2(ev.x, ev.y) / ppp)
            }
        };
        events.push(egui::Event::MouseWheel {
            unit,
            delta,
            // 滚轮无 trackpad 起止概念（egui 注释：未知即填 Move）
            phase: egui::TouchPhase::Move,
            modifiers,
        });
    }
    for ev in window_focused.read() {
        events.push(egui::Event::WindowFocused(ev.focused));
    }
    if modifiers != input.last_modifiers {
        input.last_modifiers = modifiers;
        events.push(egui::Event::ModifiersChanged(modifiers));
    }
    egui::RawInput {
        // points 域 = 窗口逻辑尺寸（左上原点，与 epaint 顶点同域）
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(window.width(), window.height()),
        )),
        time: Some(time.elapsed_secs_f64()),
        events,
        focused: window.focused,
        ..Default::default()
    }
}

/// `ButtonInput<KeyCode>` → egui 修饰键。Windows：command = ctrl、mac_cmd 恒 false。
fn modifiers_from(keys: &ButtonInput<KeyCode>) -> egui::Modifiers {
    let pressed = |code: KeyCode| keys.pressed(code);
    let ctrl =
        pressed(KeyCode::ControlLeft) || pressed(KeyCode::ControlRight);
    egui::Modifiers {
        alt: pressed(KeyCode::AltLeft) || pressed(KeyCode::AltRight),
        ctrl,
        shift: pressed(KeyCode::ShiftLeft) || pressed(KeyCode::ShiftRight),
        mac_cmd: false,
        command: ctrl,
    }
}

/// bevy `MouseButton` → egui `PointerButton`。
fn map_button(button: MouseButton) -> egui::PointerButton {
    match button {
        MouseButton::Left => egui::PointerButton::Primary,
        MouseButton::Right => egui::PointerButton::Secondary,
        MouseButton::Middle => egui::PointerButton::Middle,
        // 调试 UI 不接额外按键：Back/Forward/Other 静默映射到第一个扩展位
        _ => egui::PointerButton::Extra1,
    }
}

/// bevy `KeyCode` → egui `Key`（调试 UI 的快捷键面：字母/数字/功能键/常用标点，
/// 修饰键本身无 egui Key 对应——状态走 `ModifiersChanged`）。未映射键返回 None。
fn map_key(code: &KeyCode) -> Option<egui::Key> {
    use egui::Key as E;
    use KeyCode as B;
    Some(match code {
        B::Escape => E::Escape,
        B::Tab => E::Tab,
        B::Backspace => E::Backspace,
        B::Enter | B::NumpadEnter => E::Enter,
        B::Space => E::Space,
        B::ArrowDown => E::ArrowDown,
        B::ArrowLeft => E::ArrowLeft,
        B::ArrowRight => E::ArrowRight,
        B::ArrowUp => E::ArrowUp,
        B::Insert => E::Insert,
        B::Delete => E::Delete,
        B::Home => E::Home,
        B::End => E::End,
        B::PageUp => E::PageUp,
        B::PageDown => E::PageDown,
        B::Comma => E::Comma,
        B::Period => E::Period,
        B::Slash => E::Slash,
        B::Backslash => E::Backslash,
        B::Semicolon => E::Semicolon,
        B::Minus => E::Minus,
        B::Equal => E::Equals,
        B::BracketLeft => E::OpenBracket,
        B::BracketRight => E::CloseBracket,
        B::Backquote => E::Backtick,
        B::Digit0 | B::Numpad0 => E::Num0,
        B::Digit1 | B::Numpad1 => E::Num1,
        B::Digit2 | B::Numpad2 => E::Num2,
        B::Digit3 | B::Numpad3 => E::Num3,
        B::Digit4 | B::Numpad4 => E::Num4,
        B::Digit5 | B::Numpad5 => E::Num5,
        B::Digit6 | B::Numpad6 => E::Num6,
        B::Digit7 | B::Numpad7 => E::Num7,
        B::Digit8 | B::Numpad8 => E::Num8,
        B::Digit9 | B::Numpad9 => E::Num9,
        B::KeyA => E::A,
        B::KeyB => E::B,
        B::KeyC => E::C,
        B::KeyD => E::D,
        B::KeyE => E::E,
        B::KeyF => E::F,
        B::KeyG => E::G,
        B::KeyH => E::H,
        B::KeyI => E::I,
        B::KeyJ => E::J,
        B::KeyK => E::K,
        B::KeyL => E::L,
        B::KeyM => E::M,
        B::KeyN => E::N,
        B::KeyO => E::O,
        B::KeyP => E::P,
        B::KeyQ => E::Q,
        B::KeyR => E::R,
        B::KeyS => E::S,
        B::KeyT => E::T,
        B::KeyU => E::U,
        B::KeyV => E::V,
        B::KeyW => E::W,
        B::KeyX => E::X,
        B::KeyY => E::Y,
        B::KeyZ => E::Z,
        _ => return None,
    })
}
