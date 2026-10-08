//! 采集机制：PostUpdate 帧末直读 primitive 三样，产出 [`CollectedScene`] 快照，
//! 零 Vulkan 代码。
//!
//! 禁渲染后官方 Extract（bevy_render 的 extract_component 家族）不存在——绘制
//! 半边要用的数必须帧末自己从 World 里拿。时机钉在 PostUpdate 且排在
//! [`TransformSystems::Propagate`] 之后：那里是传播链的收口
//! （bevy_transform/src/plugins.rs:37-47），读到的 [`GlobalTransform`] 才是含
//! 父链的本帧终值。快照只带拓扑（实体 + 两柄 + 终值矩阵），顶点/贴图等内容
//! 留在资产容器——主世界 Assets 数据常驻，上传链与 DrawList 从快照出发去容器
//! 取数。
//!
//! 一次性采集核验（判定线期望值随当前场景写死）在
//! [`crate::scene::content::collect_report`]，与本机制仅以调度链序耦合。
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.1-ECS侧取数/3.1.4-采集系统：PostUpdate帧末直读与CollectedScene快照.md`

use bevy::{
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    transform::TransformSystems,
};

use ash_macros::system;

/// 采集插件：每帧重建 [`CollectedScene`]。接线收在本插件内，main 只
/// `add_plugins`，不引用内部系统。
pub struct AshCollectPlugin;

impl Plugin for AshCollectPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CollectedScene>().add_systems(
            PostUpdate,
            collect_scene.after(TransformSystems::Propagate),
        );
    }
}

/// 帧末的采集快照（PostUpdate 末"会被渲染的场景内容"）：GPU 半边（上传 pending、
/// DrawList）从这里读。每帧重建，行窄（两柄 + 一矩阵），重建成本可忽略。
#[derive(Resource, Default)]
pub struct CollectedScene {
    /// 本帧在场且三样俱全（`Mesh3d` + `GlobalTransform` + `MeshMaterial3d`）的 primitive。
    pub primitives: Vec<CollectedPrimitive>,
}

/// 快照的一行：柄去容器取内容，`model` 直进 push constant。
#[derive(Clone)]
pub struct CollectedPrimitive {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// [`GlobalTransform`] 终值矩阵（PostUpdate 传播后直读，含父链）。
    pub model: Mat4,
}

/// 采集系统（每帧）：重建 [`CollectedScene`]。`pub(crate)` 是给业务侧核验
/// （[`crate::scene::content::collect_report::report_scene_collected`]）当排序锚点用，
/// 语义 = "快照已重建完毕"。
#[system]
pub(crate) fn collect_scene(
    mut scene: ResMut<CollectedScene>,
    primitives: Query<(
        Entity,
        &Mesh3d,
        &GlobalTransform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
) {
    scene.primitives.clear();
    for (entity, mesh3d, global, material3d) in &primitives {
        scene.primitives.push(CollectedPrimitive {
            entity,
            mesh: mesh3d.0.clone(),
            material: material3d.0.clone(),
            model: global.to_matrix(),
        });
    }
}
