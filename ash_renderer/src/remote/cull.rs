//! 剔除读数与开关方法族：BRP 侧的验证通道——`cull_stats` 做 GPU 标记 vs CPU
//! 参照的逐实体对照（剔除正确性的直接证据），`cull_set_enabled` 写剔除开关
//!（数据层 A/B 的远程驱动，egui checkbox 与此同源）。
//!
//! 红线边界：两个方法都只读标记/只写开关，**不提供任何"按标记改场景"的
//! 通道**——诊断计数与对照读数永远不回灌绘制决策。
//!
//! 读数陈旧性：GPU 标记是帧槽直读（`marks_frame` 槽两帧前 compute 的输出），
//! CPU 参照按 handler 时刻的相机/账本现算——对照前提 = 相机与变换自标记
//! 产生以来未变（测试惯例：set_transform/set_camera 后等 ≥3 帧再查）。

use bevy::{math::Vec2, prelude::*};
use serde_json::{json, Value};

use ash_macros::system;

use crate::{
    common::error::VulkanError,
    scene::InstanceLedger,
    render::{
        aabb_screen_rect, aabb_visible, conservative_world_aabb, frustum_planes,
        occluded_reverse_z, occlusion_mip_level, CullDiagnostics, CullSettings, HizPyramid,
        InstanceTables, MARK_SIZE, MeshPool, OcclusionDiagnostics, OcclusionFallback,
        OcclusionSettings, OBJECT_TABLE_CAPACITY,
    },
};

use super::{
    invalid_params,
    params::{brp_view_proj, clamp_rect_to_screen, parse_params},
    readback::u32_at,
    BrpError, BrpResult,
};

/// `ash_renderer/cull_stats`：剔除读数对照（只读）。返回开关状态、dispatch
/// 上界、GPU/CPU 两列逐实体可见性与不一致清单。
///
/// # Errors
/// 相机缺席 / 标记环读失败（host-visible 契约被打破，Tier② 级）。
#[system]
pub(crate) fn cull_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // 相机缺席 = 场景装配问题，报错
    let view_proj = brp_view_proj(world, "CPU 参照无法提取视锥")?;
    let planes = frustum_planes(&view_proj);
    // 读数快照先行取值（避免与下方标记环可变借用交叠）
    let (marks_frame, bound, enabled) = {
        let diag = world.resource::<CullDiagnostics>();
        let settings = world.resource::<CullSettings>();
        (diag.marks_frame, diag.bound, settings.enabled)
    };

    // —— CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒 → 六平面求交。
    // 行先收拢（账本/pool 只读借用不与标记环写读并存）
    let pool = world.resource::<MeshPool>();
    let mut reference: Vec<(Entity, u32, bool)> = Vec::new();
    for (entity, row) in world.resource::<InstanceLedger>().rows() {
        let Some(slot) = row.instance_slot else {
            continue; // 未驻留行无标记可言（DrawList 同款暂缓语义）
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

    // —— GPU 标记：marks_frame 槽直读（fence 纪律内，handler 不等帧）
    let frame = marks_frame;
    let mut gpu = vec![0u8; OBJECT_TABLE_CAPACITY as usize * MARK_SIZE as usize];
    world
        .resource_mut::<InstanceTables>()
        .mark_ring(frame)
        .read(0, &mut gpu)
        .map_err(|e: VulkanError| BrpError::internal(format!("标记环读失败: {e}")))?;

    let mut mismatches = Vec::new();
    let mut gpu_visible = 0u32;
    let mut cpu_visible = 0u32;
    let mut rows = Vec::with_capacity(reference.len());
    let mut slots = Vec::with_capacity(reference.len());
    for (entity, slot, cpu) in reference {
        let mark = u32_at(&gpu, slot as usize * MARK_SIZE as usize)?;
        let gpu = mark == 1;
        if gpu {
            gpu_visible += 1;
        }
        if cpu {
            cpu_visible += 1;
        }
        if gpu != cpu {
            mismatches.push(json!({ "entity": format!("{entity:?}"), "slot": slot, "gpu": gpu, "cpu": cpu }));
        }
        rows.push(json!({
            "entity": format!("{entity:?}"),
            "slot": slot,
            "gpu": gpu,
            "cpu": cpu,
        }));
        slots.push(slot);
    }
    // —— 遮挡分列：读 occ 标记环（同帧槽，fence 纪律内）+ 诊断账面。
    // 遮挡判可见数 = occ=1 的在场行；GPU 标记已含遮挡合并（b3 = 视锥 ∧ 遮挡），
    // 本节单独报遮挡半边供 A/B 归因（视锥剔除量/遮挡剔除量分列）。
    let occ_frame = world.resource::<OcclusionDiagnostics>().marks_frame;
    let mut occ_bytes = vec![0u8; OBJECT_TABLE_CAPACITY as usize * MARK_SIZE as usize];
    world
        .resource_mut::<InstanceTables>()
        .occ_mark_ring(occ_frame)
        .read(0, &mut occ_bytes)
        .map_err(|e: VulkanError| BrpError::internal(format!("遮挡标记环读失败: {e}")))?;
    // 逐行找 occ 标记（与 rows 同序的槽号表）
    let mut occlusion_visible = 0u32;
    for slot in slots {
        if u32_at(&occ_bytes, slot as usize * MARK_SIZE as usize)? == 1 {
            occlusion_visible += 1;
        }
    }
    let (occlusion_settings_enabled, occlusion_dispatched, occlusion_fallback) = {
        let (diag, settings) = (
            world.resource::<OcclusionDiagnostics>(),
            world.resource::<OcclusionSettings>(),
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
        "marks_frame": frame,
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

/// `ash_renderer/cull_set_enabled` 参数。
#[derive(serde::Deserialize)]
struct SetEnabledParams {
    enabled: bool,
}

/// `ash_renderer/cull_set_enabled`：写剔除开关（true = compute dispatch，
/// false = host 全可见填充）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn cull_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetEnabledParams = parse_params(params, "params 缺失：需要 enabled")?;
    world.resource_mut::<CullSettings>().enabled = p.enabled;
    Ok(json!({ "enabled": p.enabled }))
}

/// `ash_renderer/occlusion_set_enabled`：写遮挡开关（true = 遮挡测试
/// dispatch（金字塔就绪且相机稳定时），false = host 填 b6 全可见——数据层
/// A/B 的远程驱动，egui checkbox 与此同源）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/类型错。
#[system]
pub(crate) fn occlusion_set_enabled(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetEnabledParams = parse_params(params, "params 缺失：需要 enabled")?;
    world.resource_mut::<OcclusionSettings>().enabled = p.enabled;
    Ok(json!({ "enabled": p.enabled }))
}

/// `ash_renderer/hiz_census` 参数：slot/level 可选（缺省 = 当前帧槽 / 全级）。
#[derive(serde::Deserialize)]
struct HizCensusParams {
    #[serde(default)]
    slot: Option<usize>,
    #[serde(default)]
    level: Option<u32>,
}

/// `ash_renderer/hiz_census`：Hi-Z 金字塔读回统计（金字塔 CPU 参照的读数口）。逐级拷出统计 min/max/mean 与背景（0）占比——归约方向错误
/// 或链路断裂会直接反映在统计上（如全 1/全 0/上下颠倒无 0）。**只进验证**，
/// 不进绘制路径（红线同 cull_stats）。诊断提交 = 一次性命令池 + staging，
/// 低频调用（测试脚本），不在帧循环内使用。
///
/// # Errors
/// 参数不合法 / HizPyramid 缺席（禁渲染宿主）/ 读回提交失败。
#[system]
pub(crate) fn hiz_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: HizCensusParams = parse_params(params, "params 缺失：需要对象（slot/level 可选）")?;
    let ctx = world
        .get_resource::<crate::render::Context>()
        .ok_or_else(|| BrpError::internal("Context 缺席——Vulkan 未初始化"))?;
    let hiz = world
        .get_resource::<crate::render::HizPyramid>()
        .ok_or_else(|| BrpError::internal("HizPyramid 资源缺席"))?;
    let slot = p.slot.unwrap_or(0) as usize;
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
            .map_err(|e: VulkanError| BrpError::internal(format!("Hi-Z 读回失败: {e}")))?;
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

/// `ash_renderer/occlusion_probe` 参数：entity 可选（缺省 = 全部可绘行）。
#[derive(serde::Deserialize)]
struct OcclusionProbeParams {
    #[serde(default)]
    entity: Option<Entity>,
    #[serde(default)]
    slot: Option<u32>,
}

/// `ash_renderer/occlusion_probe`：遮挡判定 CPU 参照（遮挡标记的参照件，
/// 金字塔读回的消费端）。对给定实体（缺省全部可绘行）：世界 AABB
/// 投屏（[`aabb_screen_rect`]）→ mip 层选择（[`occlusion_mip_level`]）→
/// 金字塔读回取 2×2 tap → [`occluded_reverse_z`] 保守判定——逐实体返回判定
/// 中间量（矩形/层/tap 值/occluder/物体深度上界），与 occlusion.wgsl 逐位
/// 同语义。**只进验证，不进绘制路径**（红线同 cull_stats）。诊断提交走
/// [`HizPyramid::readback_level_raw`]（一次性 staging，低频）。
///
/// # Errors
/// 参数不合法 / 相机缺席 / HizPyramid 缺席 / 读回失败。
#[system]
pub(crate) fn occlusion_probe(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: OcclusionProbeParams =
        parse_params(params, "params 缺失：需要对象（entity/slot 可选）")?;
    let view_proj = brp_view_proj(world, "投屏参照无法计算")?;
    let hiz = world
        .get_resource::<HizPyramid>()
        .ok_or_else(|| BrpError::internal("HizPyramid 资源缺席"))?;
    let ctx = world
        .get_resource::<crate::render::Context>()
        .ok_or_else(|| BrpError::internal("Context 缺席——Vulkan 未初始化"))?;
    let slot = p.slot.unwrap_or(0) as usize;
    let levels = hiz.levels();
    let screen = {
        let (w, h) = hiz.level_dims(0);
        [w as f32, h as f32]
    };
    let pool = world.resource::<MeshPool>();
    let mut rows = Vec::new();
    for (entity, row) in world.resource::<InstanceLedger>().rows() {
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
                "reason": "角点投影 w≤0（透视无意义，shader 同款早退）",
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
            .map_err(|e: VulkanError| BrpError::internal(format!("Hi-Z 读回失败: {e}")))?;
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

