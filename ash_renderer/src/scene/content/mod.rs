//! 场景侧业务层：当前场景内容——写死的模型进场请求、相机取景与灯光参数，以及
//! 随内容走的一次性核验（判定线期望值"6 primitive"等写在本层）。零 Vulkan 代码。
//!
//! 换场景 = 在 [`SceneEntryPlugin`] 组清单里增删组；机制半边（补位与采集）在
//! [`crate::scene::mechanism`]，不随内容变。

mod camera;
mod collect_report;
mod lights;
mod world_asset;

use super::mechanism::update_ledger;
use bevy::prelude::*;

/// 场景进场插件：Startup 里场景元素各自进场——WorldAsset 进场请求、相机、灯光
/// 一一解耦成组：**一个元素 = 一个 spawn + 一个就位核验（+ 专属修正）**，
/// 组与组零耦合，任意搭场景就是在本清单里增删组。采集核验排 PostUpdate，
/// 锚在机制侧 [`update_ledger`]（账本对账）之后读账本。
pub struct SceneEntryPlugin;

impl Plugin for SceneEntryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Startup,
            (
                world_asset::load_scene,
                camera::spawn_camera,
                lights::spawn_lights,
            ),
        )
        .add_systems(
            Update,
            (
                world_asset::report_scene_arrival,
                camera::report_camera_setup.run_if(run_once),
                lights::report_lights_setup.run_if(run_once),
            ),
        )
        .add_systems(
            PostUpdate,
            collect_report::report_scene_collected.after(update_ledger),
        );
    }
}

/// 等待型核验的超时（秒）：超时 warn 一次后不再核验。
const REPORT_TIMEOUT_SECS: f32 = 10.0;

/// 等待型一次性核验的进度：报一次即歇；等不到时超时 warn 一次后不再打扰。
#[derive(Default)]
struct PendingReport {
    done: bool,
    waited_secs: f32,
}

impl PendingReport {
    /// 累计本帧等待；超过 [`REPORT_TIMEOUT_SECS`] 时收账并返回 true（调用方 warn 一次）。
    fn timed_out(&mut self, delta_secs: f32) -> bool {
        self.waited_secs += delta_secs;
        if self.waited_secs > REPORT_TIMEOUT_SECS {
            self.done = true;
        }
        self.done
    }
}

/// 三维向量格式化：日志里统一两位小数，位置/前向数值一眼可比。
fn fmt_vec3(v: Vec3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}
