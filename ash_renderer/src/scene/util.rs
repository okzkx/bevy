//! 场景子模块的公共小工具（不承载业务语义，谁用谁 `use super::util`）。

use bevy::math::{Vec2, Vec3};

use crate::overlay::EguiState;

/// 三维向量格式化：日志里统一两位小数，位置/前向数值一眼可比。
pub(super) fn fmt_vec3(v: Vec3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}

/// egui 指针仲裁：光标位置上最顶层是否为非 Background 层（是 = 悬在调试
/// 面板上，鼠标让给 UI）。用 `layer_id_at` 而非 `is_pointer_over_egui()`
/// 的原因见 mechanism/camera_control 模块文档（begin_pass 集成形状下后者
/// 恒真）。光标不在场（None）= 不让。
pub(super) fn egui_wants_pointer(egui: Option<&EguiState>, cursor: Option<Vec2>) -> bool {
    match (egui, cursor) {
        (Some(state), Some(pos)) => state
            .ctx
            .layer_id_at(egui::pos2(pos.x, pos.y))
            .is_some_and(|layer| layer.order != egui::Order::Background),
        _ => false,
    }
}
