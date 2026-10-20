//! 宿主唯一相机卫兵：清掉一切不带 [`CameraOrbit`] 的 `Camera` 实体，零 Vulkan
//! 代码。
//!
//! 语义：**取景权归宿主**——World 恒恰一台带 `CameraOrbit` 的相机（Startup
//! 进场组 spawn），进场内容（glTF camera 节点等）携带的相机一律移除。多相机
//! 下渲染链/诊断链的 `Query::single()` 全崩（7.2 zero_day 实抓：内嵌 camera
//! 节点让 residency_stats/draw 拒答"World 里没有相机"），压制单点 = 保持
//! 单相机不变式，全部消费点零改动。
//!
//! 形状：Update 幂等清扫（每帧一次小查询），不用 OnAdd observer——bundle
//! spawn 时 `Camera` 与 `CameraOrbit` 的添加序不定，组件级触发会在宿主相机
//! 自 spawn 时误判；整帧后置语义下宿主相机恒有 `CameraOrbit`，天然免疫。
//! 将来要"多真相机"再改策略，现在取景权单一。

use bevy::{camera::Camera, prelude::*};

use crate::scene::mechanism::CameraOrbit;

use ash_macros::system;

/// 宿主唯一相机卫兵插件：Update 幂等清扫，接线收在本插件内，main 只
/// `add_plugins`。
pub struct AshCameraGuardPlugin;

impl Plugin for AshCameraGuardPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, guard_unique_camera);
    }
}

/// 宿主唯一相机卫兵（Update，幂等清扫）：不带 `CameraOrbit` 的 `Camera`
/// 实体 = 外来相机（进场内容携带），移除并留档一条 info——每实体一次（记
/// 已清实体索引，防逐帧刷屏）。
#[derive(Default)]
pub struct EjectedCameras {
    logged: Vec<Entity>,
}

#[system]
pub(super) fn guard_unique_camera(
    cameras: Query<(Entity, &Camera), Without<CameraOrbit>>,
    mut ejected: Local<EjectedCameras>,
    mut commands: Commands,
) {
    for (entity, _) in &cameras {
        commands.entity(entity).despawn();
        if !ejected.logged.contains(&entity) {
            ejected.logged.push(entity);
            info!("相机卫兵：外来相机 {entity:?} 移除（宿主唯一相机语义，进场内容不接管取景）");
        }
    }
}
