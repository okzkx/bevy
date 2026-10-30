//! 每帧按查询迭代顺序修改指定数量 primitive 的 Transform，用于测量不同变更规模。
//! 子集是当前查询顺序的前缀，不保证跨帧实体集合固定。

use bevy::prelude::*;

/// 每帧变更设置；默认关闭，`enabled = false` 或 `rows = 0` 时不修改实体。
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct WorkloadSettings {
    pub enabled: bool,
    /// 每帧改写的 primitive 数（迭代序前缀选取；0 = 关）。
    pub rows: usize,
}

/// 注册设置资源和每帧 Transform 更新系统。
pub struct AshWorkloadPlugin;

impl Plugin for AshWorkloadPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorkloadSettings>()
            .add_systems(Update, spin_workload);
    }
}

/// 采样窗内的注入角速度（度/秒）：足够让每帧的矩阵都变（60Hz 下每帧 0.5°），
/// 又不致旋转出视锥造成剔除读数漂移干扰对照。
const SPIN_DEG_PER_SEC: f32 = 30.0;

/// 每帧对迭代序前 `rows` 个可渲染 primitive 追加绕 Y 旋转。旋转不改 AABB
/// 包围球心距（保守包围体语义下不引入误剔）。
fn spin_workload(
    settings: Res<WorkloadSettings>,
    time: Res<Time>,
    mut transforms: Query<&mut Transform, With<Mesh3d>>,
) {
    if !settings.enabled || settings.rows == 0 {
        return;
    }
    let delta = time.delta_secs_f64() as f32 * SPIN_DEG_PER_SEC.to_radians();
    for mut transform in transforms.iter_mut().take(settings.rows) {
        transform.rotate_y(delta);
    }
}
