//! 宽高比补位（施工 3.1.3，3.6.2 拆出机制半边）：随窗口 resize 修正相机的
//! `PerspectiveProjection.aspect_ratio`，零 Vulkan 代码。
//!
//! 官方由 camera_system（bevy_render/src/camera.rs:354，渲染族已禁）随窗口建/改
//! 维护，禁后归我们——它是渲染族缺席的通用补位，管任意相机，与业务取景参数无关。
//! 宽高比错了，3.4 建 VP 矩阵时横向视野就错。
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.1-ECS侧取数/3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md`

use bevy::{camera::Projection, prelude::*, window::PrimaryWindow};

/// 宽高比补位插件（机制半边）：Update 里幂等对比-修正，接线收在本插件内，
/// main 只 `add_plugins`。
pub struct AshCameraAspectPlugin;

impl Plugin for AshCameraAspectPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync_projection_aspect);
    }
}

/// 宽高比补位（Update，幂等对比-修正）：camera_system 缺席后没人随窗口 resize
/// 更新 `PerspectiveProjection.aspect_ratio`（初值 1.0），不补位则 3.4 的画面
/// 横向拉伸。宽度/高度为 0（最小化）跳过，等恢复。
pub(super) fn sync_projection_aspect(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut cameras: Query<&mut Projection, With<Camera>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let (w, h) = (window.resolution.width(), window.resolution.height());
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let aspect = w / h;
    for mut projection in &mut cameras {
        if let Projection::Perspective(ref mut persp) = *projection
            && (persp.aspect_ratio - aspect).abs() > f32::EPSILON
        {
            let old = persp.aspect_ratio;
            persp.aspect_ratio = aspect;
            debug!("相机宽高比随窗口修正：{old:.4} → {aspect:.4}");
        }
    }
}
