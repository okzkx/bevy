//! 场景采集（施工 3.1.4）：PostUpdate 帧末直读 primitive 三样，产出 [`CollectedScene`]
//! 快照，零 Vulkan 代码。
//!
//! 渲染族禁用后，官方 Extract（bevy_render 的 extract_component 家族）不存在——
//! 第 N+1 帧绘制半边要用什么数，必须第 N 帧末自己从 World 里拿（入口篇既定架构
//! "帧末直读"）。时机钉在 PostUpdate 且排在 [`TransformSystems::Propagate`] 之后：
//! 那里是传播链的收口（mark_dirty_trees → propagate_parent_transforms →
//! sync_simple_transforms，bevy_transform/src/plugins.rs:37-47），读到的
//! [`GlobalTransform`] 才是含父链的本帧终值。快照只带拓扑（实体 + 两柄 + 终值矩阵），
//! 顶点/贴图等内容留在资产容器——禁渲染后主世界 Assets 数据常驻（施工计划 §6.9），
//! 3.2 上传与 3.4 DrawList 从快照出发去容器取数。
//!
//! 机制与证据：`.agent/docs/3-静态取数链路/3.1-ECS侧取数/3.1.4-采集系统：PostUpdate帧末直读与CollectedScene快照.md`

use std::collections::HashSet;

use bevy::{
    ecs::system::SystemParam,
    mesh::Indices,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    transform::TransformSystems,
};

use super::util::fmt_vec3;

/// 采集插件：把 [`collect_scene`]（每帧重建快照）与 [`report_scene_collected`]
///（一次性核验）按"先采后验"链进 PostUpdate（传播链之后）。接线收在本插件内，
/// main 只 `add_plugins`，不引用内部系统。
pub struct AshCollectPlugin;

impl Plugin for AshCollectPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CollectedScene>().add_systems(
            PostUpdate,
            (collect_scene, report_scene_collected)
                .chain()
                .after(TransformSystems::Propagate),
        );
    }
}

/// 第 N 帧的采集快照（PostUpdate 末"会被渲染的场景内容"）：第 N+1 帧的 GPU 半边
/// 从这里读——施工计划 §2 图中跨帧折线的 ECS 侧起点（消费者 3.2 上传 pending、
/// 3.4 DrawList）。每帧重建，行窄（两柄 + 一矩阵），重建成本可忽略。
#[derive(Resource, Default)]
pub struct CollectedScene {
    /// 本帧在场且三样俱全（`Mesh3d` + `GlobalTransform` + `MeshMaterial3d`）的 primitive。
    pub primitives: Vec<CollectedPrimitive>,
}

/// 快照的一行：柄去容器取内容，`model` 直进 push constant（3.4）。
#[derive(Clone)]
pub struct CollectedPrimitive {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// [`GlobalTransform`] 终值矩阵（PostUpdate 传播后直读，含父链）。
    pub model: Mat4,
}

/// 采集系统（每帧）：重建 [`CollectedScene`]。不随核验完成停摆——GPU 半边永远读
/// 最新一帧；核验是另一支系统（[`report_scene_collected`]），与本系统仅以链序耦合。
fn collect_scene(
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

/// 核验要读的成组数据：快照 + 层级查询 + 两个资产容器。SystemParam 打包让系统
/// 签名保持三参——官方渲染侧的 Extract 系统同样用参数束装下成排的 Query/Res。
#[derive(SystemParam)]
struct CollectData<'w, 's> {
    scene: Res<'w, CollectedScene>,
    /// primitive 全集（含尚未缝上材质的）——三样俱全与否由此比对得出。
    hierarchy: Query<
        'w,
        's,
        (
            &'static Transform,
            &'static GlobalTransform,
            Option<&'static ChildOf>,
        ),
        With<Mesh3d>,
    >,
    parents: Query<'w, 's, &'static GlobalTransform>,
    meshes: Res<'w, Assets<Mesh>>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    images: Res<'w, Assets<Image>>,
}

/// 一次性核验的进度（报一次即歇；10s 凑不齐按 Tier① warn 一次后不再打扰）。
#[derive(Default)]
struct CollectState {
    done: bool,
    waited_secs: f32,
}

/// 采集核验（报一次即歇）：实体数/每 primitive 顶点数/材质与贴图去重/传播契约。
///
/// 两 Tier：资产未到货（[`Assets::get`] 为 `None`）是异步加载的正常态，空帧容忍、
/// 下一帧再查，不是失败；primitive 已在场但 10s 仍凑不齐或契约不过才 warn 一次——
/// 根因在资产侧或缝接线（3.1.1/3.1.2），本系统不修，帧循环照常。
fn report_scene_collected(mut state: Local<CollectState>, time: Res<Time>, data: CollectData) {
    if state.done {
        return;
    }
    if data.scene.primitives.is_empty() {
        // 场景未展开：等。到货问题由 3.1.2 report_scene_arrival 告警，这里不重复。
        state.waited_secs += time.delta_secs();
        if state.waited_secs > 10.0 {
            warn!("场景采集 10s 未见 primitive 实体——进场链路问题，看 3.1.2 到货报告与加载错误日志");
            state.done = true;
        }
        return;
    }

    let total = data.hierarchy.iter().count();
    let joined = data.scene.primitives.len();
    let mut resolved = joined == total;
    let mut with_parent = 0usize;
    let mut detail = Vec::new();
    let (mut verts_sum, mut idx_sum, mut tex_slots) = (0usize, 0usize, 0usize);
    let mut tex_ids = HashSet::new();
    let mut unlit = 0usize;
    let (mut max_dt, mut max_dev) = (0.0_f32, 0.0_f32);
    for row in &data.scene.primitives {
        // 传播契约：子 GlobalTransform == 父 GlobalTransform × 本地 Transform（无父链
        // 则 == 本地）。FlightHelmet 六节点全恒等——种子值与传播值重合，本核验守的
        // 是"读在传播之后 + 数据合法"；非恒等父链的逐帧正确性由 3.4 画面判定。
        let Ok((local, global, child_of)) = data.hierarchy.get(row.entity) else {
            resolved = false;
            continue;
        };
        if child_of.is_some() {
            with_parent += 1;
        }
        let expected = match child_of {
            Some(parent) => data.parents.get(parent.0).ok().map(|p| *p * *local),
            None => Some(GlobalTransform::from(*local)),
        };
        if let Some(expected) = expected {
            let dt = (global.translation() - expected.translation()).length();
            let dev = (*global.forward())
                .angle_between(*expected.forward())
                .to_degrees();
            max_dt = max_dt.max(dt);
            max_dev = max_dev.max(dev);
        } else {
            resolved = false;
        }
        let pos = row.model.w_axis.truncate();
        let mesh_note = match data.meshes.get(&row.mesh) {
            Some(m) => {
                let (v, i) = (m.count_vertices(), m.indices().map_or(0, Indices::len));
                verts_sum += v;
                idx_sum += i;
                format!("顶点 {v} 索引 {i}")
            }
            None => {
                resolved = false;
                "Mesh 待到货".to_string()
            }
        };
        let mat_note = match data.std_materials.get(&row.material) {
            Some(m) => {
                if m.unlit {
                    unlit += 1;
                }
                let mut slots = 0usize;
                for handle in [
                    &m.base_color_texture,
                    &m.metallic_roughness_texture,
                    &m.occlusion_texture,
                    &m.normal_map_texture,
                    &m.emissive_texture,
                ]
                .into_iter()
                .flatten()
                {
                    slots += 1;
                    tex_ids.insert(handle.id());
                }
                tex_slots += slots;
                let s = m.base_color.to_srgba();
                format!(
                    "rgba({:.2}, {:.2}, {:.2}, {:.2}) 贴图槽 {slots}",
                    s.red, s.green, s.blue, s.alpha
                )
            }
            None => {
                resolved = false;
                "StandardMaterial 待到货".to_string()
            }
        };
        detail.push(format!(
            "  #{:<3} 位 {} {mesh_note}；材质 {mat_note}",
            row.entity.index(),
            fmt_vec3(pos)
        ));
    }

    if resolved && max_dt <= 1e-4 && max_dev <= 0.01 {
        info!(
            "场景采集：primitive {total}（期望 6，三样俱全 {joined}/{total}，带父链 {with_parent}/{total}）；传播契约全过（最大位移 {max_dt:.4}、最大前向夹角 {max_dev:.4}°——FlightHelmet 节点全恒等：种子即终值）；顶点合计 {verts_sum} / 索引合计 {idx_sum}；材质 {}（unlit {unlit}）贴图槽 {tex_slots} 去重 {}（容器 Image {}）\n{}",
            data.std_materials.len(),
            tex_ids.len(),
            data.images.len(),
            detail.join("\n"),
        );
        state.done = true;
    } else {
        state.waited_secs += time.delta_secs();
        if state.waited_secs > 10.0 {
            warn!(
                "场景采集 10s 未凑齐：primitive {total}，三样俱全 {joined}/{total}，传播契约（位移 {max_dt:.4}/前向夹角 {max_dev:.4}°）——根因在资产加载或缝接线（3.1.1/3.1.2），本系统不修"
            );
            state.done = true;
        }
    }
}
