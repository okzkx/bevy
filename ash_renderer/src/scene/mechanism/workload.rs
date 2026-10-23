//! 变更形态注入：压测矩阵的三种变更形态（零变化/少量变化/全动态）
//! 需要一个确定性的"每帧改 Transform"工作源——BRP 逐请求驱动撑不起全动态
//!（10 万行逐帧 HTTP 请求不可行），本模块用 BRP 开关控制的常驻 system 承担：
//! 每帧对确定子集（迭代序前 `rows` 个可渲染 primitive）做小幅旋转，走
//! Changed<Transform> → 账本增量 → 模型环重写的既有链路，零新通道。
//!
//! 边界（只测量不主张）：这是压测工作负载不是业务功能——旋转角速度、
//! 子集大小都是矩阵参数（BRP `workload` 方法写），不承载任何渲染语义；
//! 读数只进计量。子集选取用查询迭代序（archetype 序，同帧内稳定）：同一
//! 配置逐帧命中同一批实体，保证"少量变化"读数可复现。

use bevy::prelude::*;

/// 变更形态开关（BRP `workload` 方法写；`rows = 0` 或 `enabled = false` =
/// 零变化形态）。
/// 缺省 = 关（enabled=false, rows=0）——零变化形态是默认世界状态。
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct WorkloadSettings {
    pub enabled: bool,
    /// 每帧改写的 primitive 数（迭代序前缀选取；0 = 关）。
    pub rows: usize,
}

/// 变更注入插件：开关资源 + Update 每帧改写系统。
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
