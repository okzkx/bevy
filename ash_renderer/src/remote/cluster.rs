//! 透明层验证方法族（8.1.3）：BRP 侧的 cluster 对照与测试光源注入——
//! `cluster_lights` 写测试点光源列表（透明层数据载体；8.2 光源驻留形态
//! 裁决前的注入通道），`cluster_census` 做 GPU 逐 cluster 计数 vs CPU 参照
//! 的对照（判定线②的证据面）。
//!
//! 红线边界（照 cull_stats 拍板）：census 只读计数对照，**不回灌绘制决策**；
//! lights 只写测试光源数据（透明层光照输入，不触碰不透明合成语义）。
//!
//! 读数陈旧性：cluster 计数是帧槽直读（该槽两帧前 cluster_build 的输出），
//! CPU 参照按 handler 时刻的相机/光源现算——对照前提 = 相机与光源自产生
//! 以来未变（测试惯例：注入后等 ≥3 帧再查）。

use bevy::{camera::Projection, prelude::*};

use crate::common::error::VulkanError;
use crate::render::MeshPool;
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    render::{
        cluster_reference, conservative_world_aabb, ClusterGrid, ClusterTargets, Context,
        PointLightGpu, TestPointLights,
    },
    scene::InstanceLedger,
};

use super::{error_codes, invalid_params, BrpError, BrpResult};

/// `ash_renderer/cluster_lights` 参数：光源数组（空数组 = 清空，回到纯方向光）。
#[derive(serde::Deserialize)]
struct ClusterLightsParams {
    lights: Vec<LightIn>,
}

#[derive(serde::Deserialize)]
struct LightIn {
    pos: [f32; 3],
    radius: f32,
    color: [f32; 3],
    intensity: f32,
}

/// `ash_renderer/cluster_lights`：写测试点光源列表（透明层 cluster 的数据源；
/// 8.2 光源驻留形态裁决前的注入口）。写入后下一帧起生效（光源环每帧重写）。
///
/// # Errors
/// 参数不合法 / 数量超容量（显式报错不截断）。
#[system]
pub(crate) fn cluster_lights(
    In(params): In<Option<Value>>,
    mut lights: ResMut<'_, TestPointLights>,
) -> BrpResult {
    let p: ClusterLightsParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|e| invalid_params(format!("params: {e}")))?;
    if p.lights.len() > 64 {
        return Err(invalid_params(format!(
            "光源数 {} 超出容量 64（MAX_CLUSTER_LIGHTS）",
            p.lights.len()
        )));
    }
    lights.0 = p
        .lights
        .iter()
        .map(|l| PointLightGpu {
            pos_radius: [l.pos[0], l.pos[1], l.pos[2], l.radius],
            color_intensity: [l.color[0], l.color[1], l.color[2], l.intensity],
        })
        .collect();
    info!(
        "BRP 注入测试点光源 {} 盏（透明层 cluster 数据面）",
        lights.0.len()
    );
    Ok(json!({ "count": lights.0.len() }))
}

/// `ash_renderer/cluster_census`：cluster 光源列表对照（判定线②的读数口）。
/// GPU 侧 = 本帧槽 cluster_counts（两帧前 cluster_build 输出）+ 透明行计数；
/// CPU 参照 = [`cluster_reference`] 同公式现算。逐 cluster 计数对照，mismatch
/// 清单截断前 16 条。
///
/// # Errors
/// 相机缺席 / 资源缺席 / 计数读失败。
#[system]
pub(crate) fn cluster_census(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // —— 相机矩阵（与 draw_frame 同一乘序；handler 现算 = 参照口径）
    // 渲染目标 extent（cluster 网格与对照共用同一口径，随 swapchain 重建）
    let extent = world.resource::<ClusterTargets>().extent();
    let (view_proj, inv_view_proj) = {
        let mut cameras =
            world.query_filtered::<(&Projection, &GlobalTransform), With<Camera>>();
        match cameras.single(world) {
            Ok((projection, global)) => {
                let vp = projection.get_clip_from_view() * global.affine().inverse();
                (vp, vp.inverse())
            }
            Err(_) => {
                return Err(BrpError {
                    code: error_codes::COMPONENT_ERROR,
                    message: "World 里没有相机——CPU 参照无法提取视锥".to_owned(),
                    data: None,
                });
            }
        }
    };
    let light_count = world.resource::<TestPointLights>().0.len();
    let grid: ClusterGrid;
    let gpu_counts: Vec<u32>;
    let transparent_rows: u32;
    {
        let mut targets = world.resource_mut::<ClusterTargets>();
        grid = targets.grid();
        let mut bytes = vec![0u8; grid.cluster_count() as usize * 4];
        targets
            .cluster_counts(0)
            .read(0, &mut bytes)
            .map_err(|e| BrpError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("cluster 计数读失败: {e}"),
                data: None,
            })?;
        gpu_counts = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let mut t_bytes = [0u8; 4];
        targets.t_count(0).read(0, &mut t_bytes).map_err(|e| BrpError {
            code: error_codes::INTERNAL_ERROR,
            message: format!("透明行计数读失败: {e}"),
            data: None,
        })?;
        transparent_rows = u32::from_le_bytes(t_bytes);
    }
    // CPU 参照（与 cluster_build.wgsl 同公式）
    let lights = world.resource::<TestPointLights>();
    let ref_lists = cluster_reference(view_proj, inv_view_proj, extent, grid, &lights.0);
    let mut mismatch = Vec::new();
    let mut total_gpu = 0u64;
    let mut total_cpu = 0u64;
    for (ci, (gpu, cpu)) in gpu_counts.iter().zip(&ref_lists).enumerate() {
        total_gpu += u64::from(*gpu);
        total_cpu += cpu.len() as u64;
        if (*gpu as usize) != cpu.len() && mismatch.len() < 16 {
            mismatch.push(json!({ "cluster": ci, "gpu": gpu, "cpu": cpu.len() }));
        }
    }
    let mismatch_count: usize = gpu_counts
        .iter()
        .zip(&ref_lists)
        .filter(|(g, c)| (**g as usize) != c.len())
        .count();
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
/// 的对照（判定线③的读数口）。GPU 侧 = 本帧槽透明索引/键列表回读（一次性
/// staging，低频）；CPU 参照 = 账本可绘行走访（材质 alpha<1 行），键 = 包围盒
/// 世界中心视图深度（与 transparent_collect.wgsl 同公式），升序排后逐元素对照
///（键 ≈ 相等容差 1e-4 + 行集合相等）。前提：相机与变换自排序帧以来未变。
///
/// # Errors
/// 相机缺席 / 资源缺席 / 回读失败。
#[system]
pub(crate) fn transparent_sort_check(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let (view_proj, inv_view_proj) = {
        let mut cameras =
            world.query_filtered::<(&Projection, &GlobalTransform), With<Camera>>();
        match cameras.single(world) {
            Ok((projection, global)) => {
                let vp = projection.get_clip_from_view() * global.affine().inverse();
                (vp, vp.inverse())
            }
            Err(_) => {
                return Err(BrpError {
                    code: error_codes::COMPONENT_ERROR,
                    message: "World 里没有相机——CPU 参照无法提取视锥".to_owned(),
                    data: None,
                });
            }
        }
    };
    let _ = inv_view_proj;
    // —— CPU 参照：可绘行走访（与 drawlist::drawability_eval 同判定口径），
    // alpha<1 行取保守盒中心视图深度
    let pool = world.resource::<MeshPool>();
    let std_materials = world.resource::<Assets<StandardMaterial>>();
    let mut cpu_rows: Vec<(u32, f32)> = Vec::new();
    for (_, row) in world.resource::<InstanceLedger>().rows() {
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
        let (wmin, wmax) = conservative_world_aabb(
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
        );
        let center: [f32; 3] = std::array::from_fn(|k| (wmin[k] + wmax[k]) * 0.5);
        let clip = view_proj * bevy::math::Vec4::new(center[0], center[1], center[2], 1.0);
        cpu_rows.push((slot, clip.w));
    }
    cpu_rows.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("视图深度无 NaN"));
    // —— GPU 读数：索引 + 键全量回读（容量 16384），本帧槽 = 两帧前输出
    let capacity = 16384usize; // TRANSPARENT_CAPACITY
    let mut indices_bytes = vec![0u8; capacity * 4];
    let mut keys_bytes = vec![0u8; capacity * 4];
    {
        let ctx = world.resource::<Context>();
        let targets = world.resource::<ClusterTargets>();
        targets
            .readback_sort(ctx, 0, &mut indices_bytes, &mut keys_bytes)
            .map_err(|e: VulkanError| BrpError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("透明排序回读失败: {e}"),
                data: None,
            })?;
    }
    let read_u32s = |bytes: &[u8]| -> Vec<u32> {
        bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
    };
    let gpu_indices = read_u32s(&indices_bytes);
    let gpu_keys: Vec<f32> = read_u32s(&keys_bytes)
        .iter()
        .map(|w| f32::from_bits(*w))
        .collect();
    let count = gpu_keys.iter().filter(|k| k.is_finite() && **k > f32::MIN).count();
    let _ = count;
    // 真实行 = 尾部有限键段（-inf padding 沉底）；从尾部向前数 CPU 行数个
    let n = cpu_rows.len();
    let tail_start = capacity - n;
    let mut issues = Vec::new();
    if n > 0 {
        // ① 键有序（尾部段非降）
        for i in tail_start + 1..capacity {
            if gpu_keys[i] < gpu_keys[i - 1] - 1e-4 {
                issues.push(format!("键序错位 @{i}: {} < {}", gpu_keys[i], gpu_keys[i - 1]));
                break;
            }
        }
        // ② 行集合一致（槽号多重集）
        let mut gpu_set: Vec<u32> = gpu_indices[tail_start..].to_vec();
        gpu_set.sort_unstable();
        let mut cpu_set: Vec<u32> = cpu_rows.iter().map(|(s, _)| *s).collect();
        cpu_set.sort_unstable();
        if gpu_set != cpu_set {
            issues.push(format!(
                "行集合不一致: gpu={gpu_set:?} cpu={cpu_set:?}"
            ));
        }
        // ③ 键值逐位对照（CPU 排序序 vs GPU 排序序，容差 1e-4）
        for (i, (_, key)) in (tail_start..capacity).zip(cpu_rows.iter()) {
            if (gpu_keys[i] - key).abs() > 1e-4 {
                issues.push(format!("键值偏差 @行{i}: gpu={} cpu={key}", gpu_keys[i]));
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
