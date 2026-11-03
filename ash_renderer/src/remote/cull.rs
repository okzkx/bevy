//! 剔除读数与开关方法族：`cull_stats` 做 GPU 标记 vs CPU 参照的逐实体对照（剔除
//! 正确性的直接证据），`cull_set_enabled`/`occlusion_set_enabled` 写视锥/遮挡开关
//!（egui checkbox 与此同源），`hiz_census`/`occlusion_probe` 是 Hi-Z 金字塔读回
//! 与遮挡判定的 CPU 参照。
//!
//! 边界：只读标记/只写开关，不提供"按标记改场景"的通道——诊断读数不回灌绘制决策。
//!
//! 读数陈旧性：GPU 标记读 `marks_frame` 槽（两帧前 compute 的输出），CPU 参照按
//! handler 时刻的相机/账本现算——对照前提 = 相机与变换自标记产生以来未变
//!（set_transform/set_camera 后等 ≥3 帧再查）。

use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    scene::InstanceLedger,
    render::{
        aabb_screen_rect, aabb_visible, conservative_world_aabb, frustum_planes,
        occluded_reverse_z, occlusion_mip_level, Context, CullDiagnostics, CullSettings,
        HizPyramid, InstanceTables, MARK_SIZE, MeshPool, OcclusionDiagnostics,
        OcclusionFallback, OcclusionSettings, OBJECT_TABLE_CAPACITY,
    },
};

use super::{
    params::{internal_err, invalid_params, parse_enabled, parse_params, res, res_mut},
    readback::{read_buffer, u32_at},
    reference::{brp_view_proj, clamp_rect_to_screen},
};

/// 标记环全容量字节数（每槽一个 MARK_SIZE 字）。
const MARK_RING_BYTES: usize = OBJECT_TABLE_CAPACITY as usize * MARK_SIZE as usize;

/// `ash_renderer/cull_stats`：剔除读数对照（只读）。返回开关状态、dispatch
/// 上界、GPU/CPU 两列逐实体可见性与不一致清单。
///
/// # Errors
/// 相机缺席 / 标记环读失败。
#[system]
pub(crate) fn cull_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let view_proj = brp_view_proj(world, "CPU 参照无法提取视锥")?;
    let planes = frustum_planes(&view_proj);
    // 读数快照先取值，避免与下方标记环可变借用交叠
    let (marks_frame, bound, enabled) = {
        let diag = res::<CullDiagnostics>(world)?;
        let settings = res::<CullSettings>(world)?;
        (diag.marks_frame, diag.bound, settings.enabled)
    };

    // CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒 → 六平面求交。
    // 行先收拢（账本/pool 只读借用不与标记环可变借用并存）
    let pool = res::<MeshPool>(world)?;
    let mut reference: Vec<(Entity, u32, bool)> = Vec::new();
    for (entity, row) in res::<InstanceLedger>(world)?.rows() {
        let Some(slot) = row.instance_slot else {
            continue; // 未驻留行无标记可言（与 DrawList 暂缓同语义）
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let (wmin, wmax) = conservative_world_aabb(
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
        );
        let visible = aabb_visible(&planes, wmin.into(), wmax.into());
        reference.push((entity, slot, visible));
    }

    // GPU 标记：marks_frame 槽直读（handler 不等帧）
    let gpu = read_buffer(
        res_mut::<InstanceTables>(world)?.mark_ring(marks_frame),
        MARK_RING_BYTES,
        "标记环读失败",
    )?;

    let mut mismatches = Vec::new();
    let mut gpu_visible = 0u32;
    let mut cpu_visible = 0u32;
    let mut rows = Vec::with_capacity(reference.len());
    let mut slots = Vec::with_capacity(reference.len());
    for (entity, slot, cpu) in reference {
        let gpu = u32_at(&gpu, slot as usize * MARK_SIZE as usize)? == 1;
        gpu_visible += u32::from(gpu);
        cpu_visible += u32::from(cpu);
        let row = json!({
            "entity": format!("{entity:?}"),
            "slot": slot,
            "gpu": gpu,
            "cpu": cpu,
        });
        if gpu != cpu {
            mismatches.push(row.clone());
        }
        rows.push(row);
        slots.push(slot);
    }
    // 遮挡分列：GPU 标记已是视锥与遮挡的合并结果，这里单独报遮挡贡献，供 A/B
    // 归因（视锥剔除量/遮挡剔除量分列）。遮挡判可见数 = occ 标记为 1 的在场行
    let occ_frame = res::<OcclusionDiagnostics>(world)?.marks_frame;
    let occ_bytes = read_buffer(
        res_mut::<InstanceTables>(world)?.occ_mark_ring(occ_frame),
        MARK_RING_BYTES,
        "遮挡标记环读失败",
    )?;
    let mut occlusion_visible = 0u32;
    for slot in slots {
        if u32_at(&occ_bytes, slot as usize * MARK_SIZE as usize)? == 1 {
            occlusion_visible += 1;
        }
    }
    let (occlusion_settings_enabled, occlusion_dispatched, occlusion_fallback) = {
        let (diag, settings) = (
            res::<OcclusionDiagnostics>(world)?,
            res::<OcclusionSettings>(world)?,
        );
        (
            settings.enabled,
            diag.dispatched,
            diag.fallback.map(|f| match f {
                OcclusionFallback::Disabled => "disabled",
                OcclusionFallback::CullOff => "cull_off",
                OcclusionFallback::PyramidWarmup => "pyramid_warmup",
                OcclusionFallback::CameraJump => "camera_jump",
            }),
        )
    };
    Ok(json!({
        "enabled": enabled,
        "bound": bound,
        "marks_frame": marks_frame,
        "staleness_note": "GPU 标记 = marks_frame 槽两帧前 compute 输出；对照前提 = 相机与变换自标记产生以来未变",
        "gpu_visible": gpu_visible,
        "cpu_visible": cpu_visible,
        "occlusion": {
            "settings_enabled": occlusion_settings_enabled,
            "dispatched": occlusion_dispatched,
            "fallback": occlusion_fallback,
            "occlusion_visible": occlusion_visible,
            "marks_frame": occ_frame,
        },
        "rows_compared": rows.len(),
        "mismatches": mismatches,
        "rows": rows,
    }))
}

/// `ash_renderer/cull_set_enabled`：写剔除开关（true = compute dispatch，
/// false = host 全可见填充）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn cull_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let enabled = parse_enabled(params)?;
    res_mut::<CullSettings>(world)?.enabled = enabled;
    Ok(json!({ "enabled": enabled }))
}

/// `ash_renderer/occlusion_set_enabled`：写遮挡开关（true = 金字塔就绪且相机
/// 稳定时 dispatch 遮挡测试，false = host 把遮挡标记填成全可见）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn occlusion_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let enabled = parse_enabled(params)?;
    res_mut::<OcclusionSettings>(world)?.enabled = enabled;
    Ok(json!({ "enabled": enabled }))
}

/// Hi-Z 诊断读回所需的 Vulkan 上下文与金字塔（禁渲染宿主下缺席回 internal）。
fn hiz_with_context(world: &World) -> Result<(&Context, &HizPyramid), BrpError> {
    let ctx = world
        .get_resource::<Context>()
        .ok_or_else(|| BrpError::internal("Context 缺席——Vulkan 未初始化"))?;
    let hiz = world
        .get_resource::<HizPyramid>()
        .ok_or_else(|| BrpError::internal("HizPyramid 资源缺席"))?;
    Ok((ctx, hiz))
}

/// `ash_renderer/hiz_census` 参数：slot/level 可选（缺省 = 帧槽 0 / 全级）。
#[derive(serde::Deserialize)]
struct HizCensusParams {
    #[serde(default)]
    slot: Option<usize>,
    #[serde(default)]
    level: Option<u32>,
}

/// `ash_renderer/hiz_census`：Hi-Z 金字塔逐级读回统计 min/max/mean 与背景（0）
/// 占比——归约方向错误或链路断裂会直接反映在统计上（如全 1/全 0/上下颠倒无 0）。
/// 只进验证，不进绘制路径。读回走一次性命令池 + staging，仅供低频测试调用。
///
/// # Errors
/// 参数不合法 / HizPyramid 缺席（禁渲染宿主）/ 读回提交失败。
#[system]
pub(crate) fn hiz_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: HizCensusParams = parse_params(params, "params 缺失：需要对象（slot/level 可选）")?;
    let (ctx, hiz) = hiz_with_context(world)?;
    let slot = p.slot.unwrap_or(0);
    let levels = hiz.levels();
    if let Some(level) = p.level.filter(|level| *level >= levels) {
        return Err(invalid_params(format!("level {level} 越界（金字塔 {levels} 级）")));
    }
    let wanted: Vec<u32> = p
        .level
        .map(|l| vec![l])
        .unwrap_or_else(|| (0..levels).collect());
    let mut rows = Vec::with_capacity(wanted.len());
    for level in wanted {
        let census = hiz
            .readback_level(ctx, slot, level)
            .map_err(internal_err("Hi-Z 读回失败"))?;
        rows.push(json!({
            "level": census.level,
            "size": [census.width, census.height],
            "min": census.min,
            "max": census.max,
            "mean": census.mean,
            "zero_ratio": census.zero_ratio,
        }));
    }
    Ok(json!({ "slot": slot, "levels": rows }))
}

/// `ash_renderer/occlusion_probe` 参数：entity 可选（缺省 = 全部可绘行），slot
/// 可选（金字塔帧槽，缺省 0）。
#[derive(serde::Deserialize)]
struct OcclusionProbeParams {
    #[serde(default)]
    entity: Option<Entity>,
    #[serde(default)]
    slot: Option<u32>,
}

/// `ash_renderer/occlusion_probe`：遮挡判定 CPU 参照。对给定实体（缺省全部可绘
/// 行）：世界 AABB 投屏（[`aabb_screen_rect`]）→ mip 层选择（[`occlusion_mip_level`]）
/// → 金字塔读回取 2×2 tap → [`occluded_reverse_z`] 保守判定，逐实体返回判定中间量
///（矩形/层/tap 值/occluder/物体深度上界），与 occlusion.wgsl 同语义。只进验证，
/// 不进绘制路径；读回走 [`HizPyramid::readback_level_raw`]（一次性 staging，低频）。
///
/// # Errors
/// 参数不合法 / 相机缺席 / HizPyramid 缺席 / 读回失败。
#[system]
pub(crate) fn occlusion_probe(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: OcclusionProbeParams =
        parse_params(params, "params 缺失：需要对象（entity/slot 可选）")?;
    let view_proj = brp_view_proj(world, "投屏参照无法计算")?;
    let (ctx, hiz) = hiz_with_context(world)?;
    let slot = p.slot.unwrap_or(0) as usize;
    let levels = hiz.levels();
    let screen = {
        let (w, h) = hiz.level_dims(0);
        [w as f32, h as f32]
    };
    let pool = res::<MeshPool>(world)?;
    let mut rows = Vec::new();
    for (entity, row) in res::<InstanceLedger>(world)?.rows() {
        if p.entity.is_some_and(|want| entity != want) {
            continue;
        }
        let Some(instance_slot) = row.instance_slot else {
            continue;
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let Some((px_min, px_max, depth_max)) = aabb_screen_rect(
            &view_proj,
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
            screen,
        ) else {
            rows.push(json!({
                "entity": format!("{entity:?}"), "slot": instance_slot, "verdict": "visible",
                "reason": "角点投影 w≤0（透视无意义，与 shader 一致提前退出）",
            }));
            continue;
        };
        // 坑：必须与 occlusion.wgsl 同口径先钳进屏再选 mip/取 tile
        let (px_min, px_max) = clamp_rect_to_screen(px_min, px_max, screen);
        let px_w = px_max.x - px_min.x;
        let px_h = px_max.y - px_min.y;
        let level = occlusion_mip_level(px_w, px_h, levels);
        let (lw, lh, data) = hiz
            .readback_level_raw(ctx, slot, level)
            .map_err(internal_err("Hi-Z 读回失败"))?;
        // 2×2 tap（与 occlusion.wgsl 同一坐标换算：tile = px_min 缩到本级、clamp）
        let dims = Vec2::new(lw as f32, lh as f32);
        let scale = dims / Vec2::from_array(screen);
        let clamp_max = (dims - Vec2::splat(1.0)).max(Vec2::ZERO);
        let tile = (px_min * scale).clamp(Vec2::ZERO, clamp_max).as_uvec2();
        let (x1, y1) = ((tile.x + 1).min(lw.saturating_sub(1)), (tile.y + 1).min(lh.saturating_sub(1)));
        // 读回段越界（空级/尺寸不符）= 读数侧不变式破缺，回 internal
        let at = |x: u32, y: u32| {
            data.get((y as usize) * (lw as usize) + (x as usize))
                .copied()
                .ok_or_else(|| BrpError::internal(format!("Hi-Z 第 {level} 级读回越界：({x},{y}) 超出 {lw}×{lh}")))
        };
        let taps = [at(tile.x, tile.y)?, at(x1, tile.y)?, at(tile.x, y1)?, at(x1, y1)?];
        let occluder = taps.iter().copied().fold(f32::INFINITY, f32::min);
        rows.push(json!({
            "entity": format!("{entity:?}"),
            "slot": instance_slot,
            "px_rect": [px_min.x, px_min.y, px_max.x, px_max.y],
            "level": level,
            "level_size": [lw, lh],
            "taps": taps,
            "occluder": occluder,
            "obj_depth_max": depth_max,
            "occluded": occluded_reverse_z(depth_max, occluder),
        }));
    }
    Ok(json!({ "slot": slot, "rows": rows }))
}
