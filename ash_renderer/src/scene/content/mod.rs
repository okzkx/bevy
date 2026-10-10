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
                world_asset::load_flight_helmet,
                camera::spawn_camera,
                lights::spawn_lights,
            ),
        )
        .add_systems(
            Update,
            (
                world_asset::report_scene_arrival,
                camera::report_camera_setup,
                lights::report_lights_setup,
            ),
        )
        .add_systems(
            PostUpdate,
            collect_report::report_scene_collected.after(update_ledger),
        );
    }
}
