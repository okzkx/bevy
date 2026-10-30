//! 渲染插件未启用时，将主窗口宽高比同步到透视相机；窗口尺寸为零时跳过。

use bevy::{camera::Projection, prelude::*, window::PrimaryWindow};

use ash_macros::system;

/// 在 Update 中同步主窗口尺寸与透视相机宽高比。
pub struct AshCameraAspectPlugin;

impl Plugin for AshCameraAspectPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync_projection_aspect);
    }
}

/// 窗口尺寸为零时跳过更新，恢复后再同步；只处理透视投影。
#[system]
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
