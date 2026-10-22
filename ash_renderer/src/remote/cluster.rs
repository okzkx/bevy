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
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    cluster_reference, ClusterGrid, ClusterTargets, PointLightGpu, TestPointLights,
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
