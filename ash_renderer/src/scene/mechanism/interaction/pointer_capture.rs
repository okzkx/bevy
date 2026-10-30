//! overlay 发布鼠标与键盘占用状态，场景输入系统据此决定是否处理输入。
//! 写方挂 [`PointerCaptureSet`]，读方排在其后以保证同帧写后读；没有写方时默认不占用输入。

use bevy::prelude::*;

/// 本帧输入仲裁结果（写方每帧整体覆写）。
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PointerCapture {
    /// 光标位置上最顶层是 UI 浮动层（鼠标按下/滚轮让给 UI）；光标离窗为 false。
    pub over_ui: bool,
    /// UI 正在占用键盘（文本编辑中，漫游键让给 UI）。
    pub wants_keyboard: bool,
}

/// `PointerCapture` 发布者使用的系统集；输入读方应排在该集合之后。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PointerCaptureSet;
