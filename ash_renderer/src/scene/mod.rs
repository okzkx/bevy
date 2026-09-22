//! ECS 侧取数（step3 施工 3.1）：glTF 材质缝接线、场景进场、相机灯光与场景采集，零 Vulkan 代码。
//!
//! 本层只从 bevy 的资产容器与 World 里取渲染要用的数，全部系统不碰 Vulkan 符号。
//! 按职责拆五个子模块：
//!
//! | 子模块 | 职责 | 施工 |
//! |---|---|---|
//! | [`material_hook`] | 材质缝接线：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 | 3.1.1 |
//! | [`world_asset`] | 场景进场：load FlightHelmet + 到货统计 | 3.1.2 |
//! | [`camera`] | 相机组：裸 Camera 三件套 + 宽高比补位 + 就位核验 | 3.1.3 |
//! | [`lights`] | 灯光组：方向光 spawn + 环境光资源核验 | 3.1.3 |
//! | [`collect`] | 场景采集：PostUpdate 帧末直读 primitive 三样，产 CollectedScene 快照 | 3.1.4 |
//! | `util` | 子模块公共小工具 | — |
//!
//! 机制与证据：`.agent/docs/3-静态取数链路/3.1-ECS侧取数/`

mod camera;
mod collect;
mod lights;
mod material_hook;
mod util;
mod world_asset;

pub use collect::AshCollectPlugin;
pub use material_hook::AshMaterialHookPlugin;

use bevy::prelude::*;

/// 场景进场插件（施工 3.1.2~3.1.3）：Startup 里场景元素各自进场——WorldAsset 进场请求
/// （3.1.2）、相机（3.1.3）、灯光（3.1.3）一一解耦成组：**一个元素 = 一个 spawn +
/// 一个就位核验（+ 专属修正）**，组与组零耦合，以后任意搭场景就是在本清单里增删组。
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
                camera::sync_projection_aspect,
                camera::report_camera_setup,
                lights::report_lights_setup,
            ),
        );
    }
}
