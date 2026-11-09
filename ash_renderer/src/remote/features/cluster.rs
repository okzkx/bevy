//! cluster 光照与透明层方法族：`light_spawn`/`light_update`/`light_remove`/
//! `light_stats` 读写常驻灯表（槽号即稳定身份，改内容不换槽），
//! `cluster_census` 做 GPU 逐 cluster 计数 vs CPU 参照的对照，
//! `transparent_sort_check` 做 GPU 透明排序 vs CPU 深度排序的对照。
//!
//! 边界：对照方法只读，读数不回灌绘制决策；light_* 只写光源数据。
//!
//! 读数陈旧性：GPU 计数读帧槽 0（该槽最近一次 cluster_build 的输出），CPU 参照
//! 按 handler 时刻的相机/光源现算——对照前提 = 相机与光源自产生以来未变（注入后
//! 等 ≥3 帧再查）。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    cluster_reference, conservative_world_aabb, ClusterGrid, Context, LightClusters, LightRecord,
    LightTable, MeshPool, TransparentTargets, LIGHT_FLAG_SHADOW, LIGHT_TYPE_POINT,
    TRANSPARENT_CAPACITY,
};
use crate::scene::InstanceLedger;

use crate::remote::common::{
    params::{internal_err, invalid_params, parse_params, res, res_mut},
    readback::{read_buffer, u32_at, words},
    reference::brp_view_proj,
};

/// 光源注入公共参数（spawn 全量必填；update 全量可选——逐项合并）。
#[derive(serde::Deserialize)]
struct LightParams {
    pos: Option<[f32; 3]>,
    radius: Option<f32>,
    color: Option<[f32; 3]>,
    intensity: Option<f32>,
    /// shadow 标志位（写入灯表 flags）。
    shadow: Option<bool>,
}

impl LightParams {
    fn merge_onto(self, record: &mut LightRecord) {
        if let Some(pos) = self.pos {
            record.pos_radius[..3].copy_from_slice(&pos);
        }
        if let Some(radius) = self.radius {
            record.pos_radius[3] = radius;
        }
        if let Some(color) = self.color {
            record.color_intensity[..3].copy_from_slice(&color);
        }
        if let Some(intensity) = self.intensity {
            record.color_intensity[3] = intensity;
        }
        match self.shadow {
            Some(true) => record.flags |= LIGHT_FLAG_SHADOW,
            Some(false) => record.flags &= !LIGHT_FLAG_SHADOW,
            None => {}
        }
    }
}

/// `ash_renderer/light_spawn`：spawn 一盏点光进常驻表，返回槽号（cluster
/// 列表引用的稳定身份）。写入后下帧起生效（脏行增量重写）。
///
/// # Errors
/// 参数缺失 / 半径非正 / 容量耗尽（显式报错不截断）。
#[system]
pub(crate) fn light_spawn(
    In(params): In<Option<Value>>,
    mut table: ResMut<'_, LightTable>,
) -> BrpResult {
    let p: LightParams = parse_params(params, "params 缺失：需要 pos 与 color")?;
    let pos = p
        .pos
        .ok_or_else(|| invalid_params("params 缺失：需要 pos（光源世界位置）"))?;
    let color = p
        .color
        .ok_or_else(|| invalid_params("params 缺失：需要 color（线性 RGB）"))?;
    let radius = p.radius.unwrap_or(10.0); // 缺省作用半径（米）
    let intensity = p.intensity.unwrap_or(10.0);
    if radius <= 0.0 {
        return Err(invalid_params(format!(
            "radius {radius} 非正——作用半径必须大于 0"
        )));
    }
    let mut record = LightRecord {
        pos_radius: [pos[0], pos[1], pos[2], radius],
        color_intensity: [color[0], color[1], color[2], intensity],
        flags: LIGHT_TYPE_POINT << 8, // 类型字段在 flags 的 bit 8 起
    };
    // 其余项已写入，这里只为合并 shadow 标志
    p.merge_onto(&mut record);
    let slot = table
        .spawn(record)
        .map_err(internal_err("光源 spawn 失败"))?;
    info!("BRP spawn 点光 → 常驻表槽 {slot}（radius {radius}）");
    Ok(json!({ "slot": slot, "count": table.light_count() }))
}

/// `ash_renderer/light_update` 参数：槽号 + 逐项可选合并。
#[derive(serde::Deserialize)]
struct LightUpdateParams {
    slot: u32,
    #[serde(flatten)]
    changes: LightParams,
}

/// `ash_renderer/light_update`：原位改一盏灯的参数（槽号不变——cluster 列表
/// 引用稳定，改内容不换槽）。写入后下帧起生效。
///
/// # Errors
/// 参数不合法 / 槽号不在场。
#[system]
pub(crate) fn light_update(
    In(params): In<Option<Value>>,
    mut table: ResMut<'_, LightTable>,
) -> BrpResult {
    let p: LightUpdateParams = parse_params(params, "params 缺失：需要 slot")?;
    let mut record = table
        .record(p.slot)
        .ok_or_else(|| invalid_params(format!("光源槽 {} 不在场", p.slot)))?;
    p.changes.merge_onto(&mut record);
    table
        .update(p.slot, record)
        .map_err(internal_err("光源 update 失败"))?;
    Ok(json!({ "slot": p.slot }))
}

/// `ash_renderer/light_remove`：回收一盏灯（槽清零入 free list，cluster 列表
/// 下一帧起不再命中）。
///
/// # Errors
/// 参数不合法 / 槽号不在场。
#[system]
pub(crate) fn light_remove(
    In(params): In<Option<Value>>,
    mut table: ResMut<'_, LightTable>,
) -> BrpResult {
    #[derive(serde::Deserialize)]
    struct P {
        slot: u32,
    }
    let p: P = parse_params(params, "params 缺失：需要 slot")?;
    table
        .remove(p.slot)
        .map_err(internal_err("光源 remove 失败"))?;
    Ok(json!({ "slot": p.slot, "count": table.light_count() }))
}

/// `ash_renderer/light_stats`：常驻表读数（在场槽/记录镜像/账面）——census
/// 前置核对与灯表对照源。
///
/// # Errors
/// 灯表资源缺席（系统参数校验失败，bevy_remote 回 internal）。
#[system]
pub(crate) fn light_stats(In(_params): In<Option<Value>>, table: Res<'_, LightTable>) -> BrpResult {
    let lights: Vec<Value> = table
        .active_slots()
        .into_iter()
        .filter_map(|slot| {
            table.record(slot).map(|r| {
                json!({
                    "slot": slot,
                    "pos": [r.pos_radius[0], r.pos_radius[1], r.pos_radius[2]],
                    "radius": r.pos_radius[3],
                    "color": [r.color_intensity[0], r.color_intensity[1], r.color_intensity[2]],
                    "intensity": r.color_intensity[3],
                    "shadow": r.flags & LIGHT_FLAG_SHADOW != 0,
                    "type": r.light_type(),
                })
            })
        })
        .collect();
    Ok(json!({
        "count": table.light_count(),
        "active": lights.len(),
        "capacity": 256, // 灯表容量（= render 内 LIGHT_TABLE_CAPACITY，未导出）
        "dirty": table.dirty_count(),
        "dirty_writes_total": table.dirty_writes_total(),
        "lights": lights,
    }))
}

/// `ash_renderer/cluster_census`：cluster 光源列表对照读数口。
/// GPU 侧 = 帧槽 0 的 cluster_counts + 透明行计数；CPU 参照 = [`cluster_reference`]
/// 同公式现算。逐 cluster 计数对照，mismatch 清单截断前 16 条。
///
/// # Errors
/// 相机缺席 / 计数读失败。
#[system]
pub(crate) fn cluster_census(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // 渲染目标 extent：cluster 网格与对照共用，随 swapchain 重建
    let extent = res::<LightClusters>(world)?.extent();
    let view_proj = brp_view_proj(world, "CPU 参照无法提取视锥")?;
    let inv_view_proj = view_proj.inverse();
    let light_count = res::<LightTable>(world)?.light_count();
    let (grid, gpu_counts): (ClusterGrid, Vec<u32>) = {
        let mut clusters = res_mut::<LightClusters>(world)?;
        let grid = clusters.grid();
        let len = grid.cluster_count() as usize * 4;
        (
            grid,
            words(&read_buffer(clusters.counts(0), len, "cluster 计数读失败")?),
        )
    };
    let transparent_rows = u32_at(
        &read_buffer(
            res_mut::<TransparentTargets>(world)?.count(0),
            4,
            "透明行计数读失败",
        )?,
        0,
    )?;
    // CPU 参照（与 cluster_build.wgsl 同公式）：按槽号铺满遍历域，空槽 = 死灯
    // 记录（radius 0，双侧一致跳过）
    let table = res::<LightTable>(world)?;
    let lights: Vec<LightRecord> = (0..light_count)
        .map(|slot| table.record(slot).unwrap_or(LightRecord::DEAD))
        .collect();
    let ref_lists = cluster_reference(view_proj, inv_view_proj, extent, grid, &lights);
    let mut mismatch = Vec::new();
    let mut mismatch_count = 0usize;
    let mut total_gpu = 0u64;
    let mut total_cpu = 0u64;
    for (ci, (gpu, cpu)) in gpu_counts.iter().zip(&ref_lists).enumerate() {
        total_gpu += u64::from(*gpu);
        total_cpu += cpu.len() as u64;
        if (*gpu as usize) != cpu.len() {
            mismatch_count += 1;
            if mismatch.len() < 16 {
                mismatch.push(json!({ "cluster": ci, "gpu": gpu, "cpu": cpu.len() }));
            }
        }
    }
    Ok(json!({
        "grid": [grid.tiles_x, grid.tiles_y, grid.slices],
        "light_count": light_count,
        "transparent_rows_gpu": transparent_rows,
        "total_assignments_gpu": total_gpu,
        "total_assignments_cpu": total_cpu,
        "mismatch_clusters": mismatch_count,
        "mismatch_sample": mismatch,
        "match": mismatch_count == 0 && total_gpu == total_cpu,
    }))
}

/// `ash_renderer/transparent_sort_check`：GPU bitonic 排序 vs CPU 深度排序参照
/// 的对照读数口。GPU 侧 = 帧槽 0 透明索引/键列表回读（一次性 staging，低频）；
/// CPU 参照 = 账本可绘行中材质 alpha<1 的行，键 = 包围盒世界中心视图深度（与
/// transparent_collect.wgsl 同公式），升序排后逐元素对照（键容差 1e-4 + 行集合
/// 相等）。前提：相机与变换自排序帧以来未变。
///
/// # Errors
/// 相机缺席 / 回读失败。
#[system]
pub(crate) fn transparent_sort_check(
    In(_params): In<Option<Value>>,
    world: &mut World,
) -> BrpResult {
    let view_proj = brp_view_proj(world, "CPU 参照无法提取视锥")?;
    // CPU 参照：可绘行判定与 drawlist 组装同口径，alpha<1 行取保守盒中心视图深度
    let pool = res::<MeshPool>(world)?;
    let std_materials = res::<Assets<StandardMaterial>>(world)?;
    let mut cpu_rows: Vec<(u32, f32)> = Vec::new();
    for (_, row) in res::<InstanceLedger>(world)?.rows() {
        let Some(slot) = row.instance_slot else {
            continue;
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let Some(material) = std_materials.get(&row.material) else {
            continue;
        };
        if material.base_color.to_linear().alpha >= 1.0 {
            continue;
        }
        let (wmin, wmax) =
            conservative_world_aabb(&row.model, mesh_slot.local_aabb.0, mesh_slot.local_aabb.1);
        let center: [f32; 3] = std::array::from_fn(|k| (wmin[k] + wmax[k]) * 0.5);
        let clip = view_proj * Vec4::new(center[0], center[1], center[2], 1.0);
        cpu_rows.push((slot, clip.w));
    }
    cpu_rows.sort_by(|a, b| a.1.total_cmp(&b.1));
    // GPU 读数：索引 + 键按透明行容量全量回读
    let capacity = TRANSPARENT_CAPACITY as usize;
    let mut indices_bytes = vec![0u8; capacity * 4];
    let mut keys_bytes = vec![0u8; capacity * 4];
    res::<TransparentTargets>(world)?
        .readback_sort(
            res::<Context>(world)?,
            0,
            &mut indices_bytes,
            &mut keys_bytes,
        )
        .map_err(internal_err("透明排序回读失败"))?;
    let gpu_indices = words(&indices_bytes);
    let gpu_keys: Vec<f32> = words(&keys_bytes)
        .iter()
        .map(|w| f32::from_bits(*w))
        .collect();
    // 真实行 = 尾部有限键段（-inf padding 沉底）；从尾部向前数 CPU 行数个
    let n = cpu_rows.len();
    let tail_start = capacity.saturating_sub(n); // 超容量时行集合必不一致，由下方集合检查报出
    let tail_keys = gpu_keys.get(tail_start..).unwrap_or_default();
    let mut issues = Vec::new();
    if n > 0 {
        // 键有序（尾部段非降）
        for (k, pair) in tail_keys.windows(2).enumerate() {
            let &[prev, cur] = pair else { continue };
            if cur < prev - 1e-4 {
                issues.push(format!("键序错位 @{}: {cur} < {prev}", tail_start + k + 1));
                break;
            }
        }
        // 行集合一致（槽号多重集）
        let mut gpu_set: Vec<u32> = gpu_indices.get(tail_start..).unwrap_or_default().to_vec();
        gpu_set.sort_unstable();
        let mut cpu_set: Vec<u32> = cpu_rows.iter().map(|(s, _)| *s).collect();
        cpu_set.sort_unstable();
        if gpu_set != cpu_set {
            issues.push(format!("行集合不一致: gpu={gpu_set:?} cpu={cpu_set:?}"));
        }
        // 键值逐位对照（CPU 排序序 vs GPU 排序序，容差 1e-4）
        for (k, (gpu_key, (_, key))) in tail_keys.iter().zip(cpu_rows.iter()).enumerate() {
            if (gpu_key - key).abs() > 1e-4 {
                issues.push(format!(
                    "键值偏差 @行{}: gpu={gpu_key} cpu={key}",
                    tail_start + k
                ));
                break;
            }
        }
    }
    Ok(json!({
        "transparent_rows_cpu": n,
        "sorted": issues.is_empty(),
        "issues": issues.into_iter().take(8).collect::<Vec<_>>(),
    }))
}
