//! 场景侧业务层（3.6.2 分层定案）：当前 M2 场景内容——写死的 FlightHelmet 进场
//! 请求、相机取景与灯光参数，以及随内容走的施工核验（报一次即歇的判定线系统，
//! 期望值"6 primitive"等写在本层）。零 Vulkan 代码。
//!
//! 换场景 = 在 [`SceneEntryPlugin`] 组清单里增删组；机制半边（补位与采集）在
//! [`crate::scene::mechanism`]，不随内容变。

mod camera;
mod collect_report;
mod lights;
mod world_asset;

use super::mechanism::collect_scene;
use bevy::prelude::*;

/// 场景进场插件（施工 3.1.2~3.1.3）：Startup 里场景元素各自进场——WorldAsset 进场请求
/// （3.1.2）、相机（3.1.3）、灯光（3.1.3）一一解耦成组：**一个元素 = 一个 spawn +
/// 一个就位核验（+ 专属修正）**，组与组零耦合，以后任意搭场景就是在本清单里增删组。
/// 采集核验（3.1.4 判定线）排 PostUpdate，锚在机制侧 [`collect_scene`]（快照重建）
/// 之后读快照。
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
            collect_report::report_scene_collected.after(collect_scene),
        );
    }
}
