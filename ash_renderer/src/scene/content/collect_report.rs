//! 采集核验（业务侧）：读 [`crate::scene::InstanceLedger`] 账本与资产容器做一次性
//! 采集核验——期望值（"期望 103"等）随当前场景写死，账本机制本体不背这些数。

use std::collections::HashSet;

use bevy::{ecs::system::SystemParam, mesh::Indices, pbr::StandardMaterial, prelude::*};

use ash_macros::system;

use super::{fmt_vec3, PendingReport};
use crate::scene::{mechanism::material_texture_slots, InstanceLedger};

/// 核验要读的成组数据：账本 + 层级查询 + 三个资产容器。
#[derive(SystemParam)]
pub(super) struct CollectData<'w, 's> {
    ledger: Res<'w, InstanceLedger>,
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

/// 采集核验（报一次即歇）：实体数/每 primitive 顶点数/材质与贴图去重/传播契约。
///
/// 资产未到货（[`Assets::get`] 为 `None`）是异步加载的正常态，下一帧再查；
/// primitive 已在场但超时仍凑不齐或契约不过才 warn 一次——根因在资产加载或
/// 材质缝接线，本系统不修，帧循环照常。
#[system]
pub(super) fn report_scene_collected(
    mut state: Local<PendingReport>,
    time: Res<Time>,
    data: CollectData,
) {
    if state.done {
        return;
    }
    if data.ledger.is_empty() {
        // 场景未展开：等；到货细节由进场到货报告（super::world_asset）告警。
        if state.timed_out(time.delta_secs()) {
            warn!("场景采集 10s 未见 primitive 实体——进场链路问题，看到货报告与加载错误日志");
        }
        return;
    }

    let total = data.hierarchy.iter().count();
    let joined = data.ledger.len();
    let mut resolved = joined == total;
    let mut with_parent = 0usize;
    let mut detail = Vec::new();
    let (mut verts_sum, mut idx_sum, mut tex_slots) = (0usize, 0usize, 0usize);
    let mut tex_ids = HashSet::new();
    let mut unlit = 0usize;
    let (mut max_dt, mut max_dev) = (0.0_f32, 0.0_f32);
    for (entity, row) in data.ledger.rows() {
        // 传播契约：子 GlobalTransform == 父 GlobalTransform × 本地 Transform（无父链
        // 则 == 本地）。Sponza 单节点带 scale 0.008，种子值 ≠ 传播值，能验到真实传播。
        let Ok((local, global, child_of)) = data.hierarchy.get(entity) else {
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
                for id in material_texture_slots(m) {
                    slots += 1;
                    tex_ids.insert(id);
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
            entity.index(),
            fmt_vec3(pos)
        ));
    }

    if resolved && max_dt <= 1e-4 && max_dev <= 0.01 {
        info!(
            "场景采集：primitive {total}（期望 103，三样俱全 {joined}/{total}，带父链 {with_parent}/{total}）；传播契约全过（最大位移 {max_dt:.4}、最大前向夹角 {max_dev:.4}°——Sponza 单节点 scale 0.008 非恒等：传播值覆盖种子值）；顶点合计 {verts_sum} / 索引合计 {idx_sum}；材质 {}（unlit {unlit}）贴图槽 {tex_slots} 去重 {}（容器 Image {}）\n{}",
            data.std_materials.len(),
            tex_ids.len(),
            data.images.len(),
            detail.join("\n"),
        );
        state.done = true;
    } else if state.timed_out(time.delta_secs()) {
        warn!(
            "场景采集 10s 未凑齐：primitive {total}，三样俱全 {joined}/{total}，传播契约（位移 {max_dt:.4}/前向夹角 {max_dev:.4}°）——根因在资产加载或材质缝接线，本系统不修"
        );
    }
}
