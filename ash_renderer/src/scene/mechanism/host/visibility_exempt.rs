//! 渲染器不读取 Bevy 可见性组件；移除三件套以避免后续可见性处理。
//! GlobalTransform 传播不依赖这些组件。
//!
//! 坑：Commands 在调度末应用，新实体本帧仍会经过可见性处理。

use bevy::prelude::*;

use crate::scene::mechanism::update_ledger;

use ash_macros::system;

/// 豁免计量（BRP 读数面走 increment_stats 不经过这里，仅日志收账用）。
#[derive(Resource, Default)]
pub struct VisibilityExemptStats {
    /// 移除三件套的累计次数。
    pub removed_total: u64,
}

/// 移除仍持有可见性组件的实体上的三件套。
#[system]
pub(crate) fn visibility_exempt(
    mut commands: Commands,
    targets: Query<Entity, With<Visibility>>,
    mut stats: ResMut<VisibilityExemptStats>,
) {
    let mut count = 0u64;
    for entity in &targets {
        commands
            .entity(entity)
            .remove::<Visibility>()
            .remove::<InheritedVisibility>()
            .remove::<ViewVisibility>();
        count += 1;
    }
    if count > 0 {
        stats.removed_total += count;
        info!("可见性豁免：移除 {count} 个实体的 Visibility 三件套（累计 {}）", stats.removed_total);
    }
}

/// 初始化计量资源，并在账本对账后运行可见性清理。
pub struct AshVisibilityExemptPlugin;

impl Plugin for AshVisibilityExemptPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VisibilityExemptStats>().add_systems(
            PostUpdate,
            visibility_exempt.after(update_ledger),
        );
    }
}
