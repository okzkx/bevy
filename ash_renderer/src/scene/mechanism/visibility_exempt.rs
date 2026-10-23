//! bevy 可见性集群豁免：把场景实体的可见性三件套
//!（`Visibility`/`InheritedVisibility`/`ViewVisibility`）移除，让 bevy 的
//! PostUpdate 可见性集群（calculate_bounds/visibility_propagate/reset_view_
//! visibility/check_visibility_cpu_culling/mark_newly_hidden）对它们整体失明。
//!
//! 依据（zero_day 7160 实体的归因实测）：PostUpdate ~8ms 的大头是可见性
//! 集群 ~5.8ms/帧（check_visibility_cpu_culling 3.75ms + visibility_propagate
//! 1.1ms + reset 0.39 + mark_newly_hidden 0.33 + calculate_bounds 0.17）——每帧
//! 对全部 primitive 重算 CPU 可见性。而本渲染器**零消费**这三件套（全仓 grep
//! 无引用）：可见性归 GPU 剔除/LOD/遮挡自算，宿主没有 bevy
//! 渲染管线，`ViewVisibility` 无人读。整条集群是纯浪费的每帧 O(N)。
//!
//! 治理形状（业务侧，不 fork 引擎）：逐实体幂等移除三件套——移除后查询不再
//! 命中，稳态帧集群空转（O(0)）。新入场景实体（BRP spawn/glTF spawn 波）会多
//! 付一帧可见性成本，下帧本系统捕获归零。相机实体不豁外——frame.rs 只读
//! Projection/GlobalTransform，可见性组件对它同样是死重；整场景失明后
//! check_visibility 连"视图"都不再命中，集群退化为空查询走访。
//!
//! 传播正确性不回归：`GlobalTransform` 传播链（TransformSystems::Propagate）
//! 不依赖 Visibility；账本对账/树窗/BRP 读的都是几何与名字，不读可见性。

use bevy::prelude::*;

use ash_macros::system;

/// 豁免计量（BRP 读数面走 increment_stats 不经过这里，仅日志收账用）。
#[derive(Resource, Default)]
pub struct VisibilityExemptStats {
    /// 已移除三件套的实体累计（含重复捕获，幂等）。
    pub removed_total: u64,
}

/// 可见性三件套移除系统（PostUpdate）：对仍持有 `Visibility` 的实体移除
/// 三件套。Commands 落地在本帧调度末——新入场景实体下帧生效（一帧可见性
/// 成本，幂等无累积）。移除后实体不再被集群查询命中，稳态帧空转。
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

/// 豁免插件：资源 + 系统挂 PostUpdate（时序与可见性集群无约束——Commands
/// 延迟落地，本帧集群已跑完，移除自下帧生效）。
pub struct AshVisibilityExemptPlugin;

impl Plugin for AshVisibilityExemptPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VisibilityExemptStats>().add_systems(
            PostUpdate,
            visibility_exempt.after(super::ledger::update_ledger),
        );
    }
}
