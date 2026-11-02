//! 宿主只保留带 [`CameraOrbit`] 的相机。glTF 等场景内容可带额外相机；多相机会让
//! 依赖 `Query::single()` 的帧循环或诊断路径失败。
//!
//! 在 Update 清理，避免 bundle 添加顺序不确定时 observer 误删宿主相机。
//! 需要支持多台相机时，必须调整此策略。

use bevy::{camera::Camera, prelude::*};

use crate::scene::mechanism::CameraOrbit;

use ash_macros::system;

/// 注册外来相机清理系统。
pub struct AshCameraGuardPlugin;

impl Plugin for AshCameraGuardPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, guard_unique_camera);
    }
}

#[system]
fn guard_unique_camera(
    cameras: Query<Entity, (With<Camera>, Without<CameraOrbit>)>,
    mut commands: Commands,
) {
    // 外来相机当帧就删，同一实体不会再出现，日志无需去重
    for entity in &cameras {
        commands.entity(entity).despawn();
        info!("相机卫兵：外来相机 {entity:?} 移除（宿主唯一相机语义，进场内容不接管取景）");
    }
}
