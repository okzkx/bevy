//! 指针/键盘仲裁窄接口：调试 UI 每帧发布"指针是否悬在 UI 上、键盘是否被 UI
//! 占用"，场景侧输入控制（相机/日光拖拽、漫游键）只读本资源决定让不让。
//!
//! 写方（overlay）的系统挂 [`PointerCaptureSet`]，读方排在该 set 之后——同帧
//! 写后读。无写方（overlay 未装）时保持默认 false = 不让，输入全归场景。

use bevy::prelude::*;

/// 本帧输入仲裁结果（写方每帧整体覆写）。
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PointerCapture {
    /// 光标位置上最顶层是 UI 浮动层（鼠标按下/滚轮让给 UI）；光标离窗为 false。
    pub over_ui: bool,
    /// UI 正在占用键盘（文本编辑中，漫游键让给 UI）。
    pub wants_keyboard: bool,
}

/// 写方系统集：[`PointerCapture`] 的发布者挂入，读方 `.after` 它。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PointerCaptureSet;
