//! 场景子模块的公共小工具（不承载业务语义，谁用谁 `use super::util`）。

use bevy::math::Vec3;

/// 三维向量格式化：日志里统一两位小数，位置/前向数值一眼可比。
pub(super) fn fmt_vec3(v: Vec3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}
