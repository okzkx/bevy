//! 表面缓存方法族（9.5）：`gi_set` 写开关/视图/视图形态/法线来源/整理波数与 Volume 规格
//! （规格只在关态改，改后重建表），`gi_stats` 读池水位/计数/显存/`gi_ms`，`gi_dump` 把
//! LDR 帧（含 patch 视图）拷回写 PPM，`gi_census` 做 cell↔patch 双向一致与位置/桶归属对账。
//! GI 关时 stats/dump/census 回 `enabled:false` 形态（不报错，脚本靠这个判分支）。

use ash_macros::system;
use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use crate::remote::common::{
    params::{internal_err, invalid_params, parse_params_or_default, res, res_mut},
    readback::{f32_at, read_buffer, u32_at, words},
};
use crate::render::{
    gi_cell_bucket, gi_cell_cascade, gi_cell_storage, gi_direction_bucket, gi_storage_to_volume,
    gi_volume_world_center, gi_voxel_size, Context, FramePool, GiSetLayout, GiSettings, GpuTimer,
    SurfaceCacheConfig, SurfaceCacheTables, COUNTER_ALLOC, COUNTER_DEFRAG_MOVED,
    COUNTER_EMISSIVE_TRIANGLES, COUNTER_EST_BOUNCE_HITS, COUNTER_EST_BOUNCE_MISSES,
    COUNTER_EST_BOUNCE_REQUESTS, COUNTER_EST_HITS, COUNTER_EST_PATCHES, COUNTER_EST_RAYS,
    COUNTER_EST_SAMPLES, COUNTER_FAILED, COUNTER_FREED, COUNTER_VIEW_COVERED,
    COUNTER_VIEW_DEVIATION, COUNTER_VIEW_HOLES, GI_COUNTER_WORDS, GI_LIGHT_SAMPLE_CAPACITY,
    GI_LIGHT_SAMPLE_SIZE, GI_UBO_SIZE, INVALID_PATCH, PATCH_GEOMETRY_SIZE, PATCH_IRRADIANCE_SIZE,
    PATCH_STATISTICS_SIZE, RING_ACTIVE, RING_CONFIGS, RING_CONFIG_WORDS, RING_FLIP, RING_WATERMARK,
};

/// `gi_set` 参数：所有字段可省（缺省不动）；规格四件只允许在关态改。
#[derive(serde::Deserialize, Default)]
struct GiSetParams {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    view: Option<u32>,
    #[serde(default)]
    form: Option<u32>,
    #[serde(default)]
    normal_source: Option<u32>,
    #[serde(default)]
    seed: Option<bool>,
    #[serde(default)]
    defrag_waves: Option<u32>,
    /// 9.6 估计开关。
    #[serde(default)]
    estimation: Option<bool>,
    /// 9.6 每 patch 每帧样本档（1/2/4/8）。
    #[serde(default)]
    samples: Option<u32>,
    /// 9.6 预热倍率（1/2/4/8）。
    #[serde(default)]
    warmup: Option<u32>,
    /// 9.6 方差引导时域混合的短时滞后（0.5～0.99）。
    #[serde(default)]
    short_hysteresis: Option<f32>,
    /// 9.6 多跳开关。
    #[serde(default)]
    multi_bounce: Option<bool>,
    /// 9.6 每帧射线预算。
    #[serde(default)]
    budget: Option<u32>,
    /// 9.6 自发光强度倍数。
    #[serde(default)]
    emissive_intensity: Option<f32>,
    /// 9.6 光源侧采样清单长度。
    #[serde(default)]
    light_samples: Option<u32>,
    /// 9.7.2 patch 级空间滤波开关。
    #[serde(default)]
    spatial_filter: Option<bool>,
    /// 9.7.2 切平面采样半径（体素倍数）。
    #[serde(default)]
    spatial_radius: Option<f32>,
    /// 9.7.4 每 1/4 像素的查表样本数（1..3）。
    #[serde(default)]
    lookup_samples: Option<u32>,
    /// 9.7.5 1/4 目标去噪遍数（0..=4）。
    #[serde(default)]
    denoise_passes: Option<u32>,
    /// 9.7.5 上采样退回纯双线性（A/B）。
    #[serde(default)]
    upsample_bilinear: Option<bool>,
    /// 9.7.6 SCGI 注入开关（关 = 整条屏幕链不录、合成回 IBL 基线）。
    #[serde(default)]
    scgi: Option<bool>,
    /// 9.7.3 patch 级时域后滤波开关。
    #[serde(default)]
    temporal_filter: Option<bool>,
    /// 9.7.3 时域平滑度 [0,1]。
    #[serde(default)]
    temporal_smoothing: Option<f32>,
    /// 最粗级立方外沿（米）。
    #[serde(default)]
    size: Option<f32>,
    #[serde(default)]
    res: Option<u32>,
    #[serde(default)]
    cascades: Option<u32>,
    #[serde(default)]
    capacity: Option<u32>,
}

fn settings_json(s: &GiSettings) -> Value {
    json!({
        "enabled": s.enabled,
        "view": s.view,
        "form": s.form,
        "normal_source": s.normal_source,
        "seed": s.seed,
        "defrag_waves": s.defrag_waves,
        // 9.6 估计参数（面板与 BRP 同源）
        "estimation": s.estimation,
        "samples": s.samples,
        "warmup": s.warmup,
        "short_hysteresis": s.short_hysteresis,
        "multi_bounce": s.multi_bounce,
        "budget": s.budget,
        "emissive_intensity": s.emissive_intensity,
        "light_samples": s.light_samples,
        "spatial_filter": s.spatial_filter,
        "spatial_radius": s.spatial_radius,
        "lookup_samples": s.lookup_samples,
        "denoise_passes": s.denoise_passes,
        "upsample_bilinear": s.upsample_bilinear,
        "scgi": s.scgi,
        "temporal_filter": s.temporal_filter,
        "temporal_smoothing": s.temporal_smoothing,
        "config": {
            "voxel_min": s.config.voxel_min,
            "res": s.config.res,
            "cascades": s.config.cascades,
            "capacity": s.config.capacity,
            "size": s.config.size(),
        },
    })
}

/// # Errors
/// 参数越界、规格在开态被改、规格非法或重建表失败。
#[system]
pub(crate) fn gi_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GiSetParams = parse_params_or_default(params)?;
    if p.view.is_some_and(|v| v > 13) {
        return Err(invalid_params(
            "view 越界（0 关 / 1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 / 6 池水位 / 7 位置偏离 /              8 cell 归属 / 9 样本档与稀释 / 10 辐照度 / 11 方差 / 12 更新次数 / 13 rank 分布）",
        ));
    }
    if p.form.is_some_and(|f| f > 2) {
        return Err(invalid_params(
            "form 越界（0 覆盖图 / 1 覆盖图+方片 / 2 只方片）",
        ));
    }
    if p.normal_source.is_some_and(|n| n > 1) {
        return Err(invalid_params(
            "normal_source 越界（0 平面法线 / 1 着色法线）",
        ));
    }
    if p.defrag_waves.is_some_and(|w| w > 16) {
        return Err(invalid_params("defrag_waves 越界（0..=16）"));
    }
    if p.samples.is_some_and(|v| !matches!(v, 1 | 2 | 4 | 8)) {
        return Err(invalid_params("samples 只取 1/2/4/8"));
    }
    if p.warmup.is_some_and(|v| !matches!(v, 1 | 2 | 4 | 8)) {
        return Err(invalid_params("warmup 只取 1/2/4/8"));
    }
    if p.short_hysteresis
        .is_some_and(|v| !(0.5..=0.99).contains(&v))
    {
        return Err(invalid_params("short_hysteresis 越界（0.5..=0.99）"));
    }
    if p.budget.is_some_and(|v| v == 0) {
        return Err(invalid_params("budget 必须 > 0"));
    }
    if p.light_samples.is_some_and(|v| v == 0 || v > 1024) {
        return Err(invalid_params("light_samples 越界（1..=1024）"));
    }
    if p.emissive_intensity.is_some_and(|v| v < 0.0) {
        return Err(invalid_params("emissive_intensity 必须 ≥ 0"));
    }
    if p.spatial_radius.is_some_and(|v| !(0.0..=8.0).contains(&v)) {
        return Err(invalid_params("spatial_radius 越界（0..=8，体素倍数）"));
    }
    if p.temporal_smoothing
        .is_some_and(|v| !(0.0..=1.0).contains(&v))
    {
        return Err(invalid_params("temporal_smoothing 越界（0..=1）"));
    }
    if p.lookup_samples.is_some_and(|v| !(1..=3).contains(&v)) {
        return Err(invalid_params("lookup_samples 越界（1..=3）"));
    }
    if p.denoise_passes.is_some_and(|v| v > 4) {
        return Err(invalid_params("denoise_passes 越界（0..=4）"));
    }
    let current = *res::<GiSettings>(world)?;
    let config_change =
        p.size.is_some() || p.res.is_some() || p.cascades.is_some() || p.capacity.is_some();
    if config_change {
        if current.enabled || p.enabled == Some(true) {
            return Err(invalid_params(
                "Volume/池规格只能在关态修改：先 {\"enabled\": false}，改完再开",
            ));
        }
        let res_value = p.res.unwrap_or(current.config.res);
        let cascades = p.cascades.unwrap_or(current.config.cascades);
        let capacity = p.capacity.unwrap_or(current.config.capacity);
        let config = match p.size {
            Some(size) => SurfaceCacheConfig::from_size(size, res_value, cascades, capacity),
            None => SurfaceCacheConfig {
                voxel_min: current.config.voxel_min,
                res: res_value,
                cascades,
                capacity,
            },
        };
        let problems = config.problems();
        if !problems.is_empty() {
            return Err(invalid_params(format!("规格非法：{}", problems.join("；"))));
        }
        if config != current.config {
            // 表可能与在飞帧同在：先排空再换
            {
                let ctx = res::<Context>(world)?;
                unsafe { ctx.device.device_wait_idle() }
                    .map_err(internal_err("GI 表重建前排空失败"))?;
            }
            let layout = res::<GiSetLayout>(world)?.raw();
            let tables = {
                let ctx = res::<Context>(world)?;
                SurfaceCacheTables::new(ctx, config, layout)
                    .map_err(internal_err("GI 表重建失败"))?
            };
            world.insert_resource(tables);
            let mut settings = res_mut::<GiSettings>(world)?;
            settings.config = config;
        }
    }
    let mut settings = res_mut::<GiSettings>(world)?;
    if let Some(enabled) = p.enabled {
        settings.enabled = enabled;
    }
    if let Some(view) = p.view {
        settings.view = view;
    }
    if let Some(form) = p.form {
        settings.form = form;
    }
    if let Some(normal_source) = p.normal_source {
        settings.normal_source = normal_source;
    }
    if let Some(seed) = p.seed {
        settings.seed = seed;
    }
    if let Some(waves) = p.defrag_waves {
        settings.defrag_waves = waves;
    }
    if let Some(estimation) = p.estimation {
        settings.estimation = estimation;
    }
    if let Some(samples) = p.samples {
        settings.samples = samples;
    }
    if let Some(warmup) = p.warmup {
        settings.warmup = warmup;
    }
    if let Some(hysteresis) = p.short_hysteresis {
        settings.short_hysteresis = hysteresis;
    }
    if let Some(multi_bounce) = p.multi_bounce {
        settings.multi_bounce = multi_bounce;
    }
    if let Some(budget) = p.budget {
        settings.budget = budget;
    }
    if let Some(intensity) = p.emissive_intensity {
        settings.emissive_intensity = intensity;
    }
    if let Some(light_samples) = p.light_samples {
        settings.light_samples = light_samples;
    }
    if let Some(spatial_filter) = p.spatial_filter {
        settings.spatial_filter = spatial_filter;
    }
    if let Some(spatial_radius) = p.spatial_radius {
        settings.spatial_radius = spatial_radius;
    }
    if let Some(temporal_filter) = p.temporal_filter {
        settings.temporal_filter = temporal_filter;
    }
    if let Some(temporal_smoothing) = p.temporal_smoothing {
        settings.temporal_smoothing = temporal_smoothing;
    }
    if let Some(lookup_samples) = p.lookup_samples {
        settings.lookup_samples = lookup_samples;
    }
    if let Some(denoise_passes) = p.denoise_passes {
        settings.denoise_passes = denoise_passes;
    }
    if let Some(upsample_bilinear) = p.upsample_bilinear {
        settings.upsample_bilinear = upsample_bilinear;
    }
    if let Some(scgi) = p.scgi {
        settings.scgi = scgi;
    }
    let effective = *settings;
    Ok(settings_json(&effective))
}

/// # Errors
/// 读回失败或 GPU 计时取回失败。
#[system]
pub(crate) fn gi_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let settings = world
        .get_resource::<GiSettings>()
        .map_or_else(GiSettings::default, |s| *s);
    let slot = res::<FramePool>(world)?.current_index();
    let (ring, counters, memory, config) = {
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let ring = words(&read_buffer(
            tables.ring(),
            RING_CONFIG_WORDS * 4,
            "GI ring 配置读失败",
        )?);
        let counters = words(&read_buffer(
            tables.counters(slot),
            GI_COUNTER_WORDS * 4,
            "GI 计数缓冲读失败",
        )?);
        (ring, counters, tables.memory(), tables.config())
    };
    let flip = ring[RING_FLIP] as usize;
    let ring_start = ring[RING_CONFIGS + flip * 4];
    let ring_count = ring[RING_CONFIGS + flip * 4 + 1];
    let active = ring[RING_ACTIVE];
    let watermark = ring[RING_WATERMARK];
    let gi_ms = world
        .get_resource_mut::<GpuTimer>()
        .map_or(Ok(None), |mut timer| timer.fetch_gi(slot))
        .map_err(internal_err("GPU 表面缓存段计时取回失败"))?
        .unwrap_or(0.0);
    // 9.6.2 光源侧清单读数：GET 本帧条数与有效条数（估计关的帧两者都是 0）
    let (light_count, light_sample_total, light_sample_valid) = {
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        let light_count = u32_at(&ubo, 288)?;
        let sample_total = u32_at(&ubo, 292)?;
        let mut valid = 0u32;
        if sample_total > 0 {
            let bytes = read_buffer(
                tables.light_samples(slot),
                sample_total.min(GI_LIGHT_SAMPLE_CAPACITY) as usize * GI_LIGHT_SAMPLE_SIZE as usize,
                "光源侧清单读失败",
            )?;
            for i in 0..sample_total.min(GI_LIGHT_SAMPLE_CAPACITY) as usize {
                // 每条 64B：info 在 48..63，其中 info.y（1 有效 / 0 无效）在 52
                let base = i * 64 + 52;
                if u32_at(&bytes, base)? == 1 {
                    valid += 1;
                }
            }
        }
        (light_count, sample_total, valid)
    };
    // 9.6.5 预算与收敛读数：有效样本档/稀释步长从 UBO 读，收敛占比与平均方差抽 64 条 patch
    let (samples_effective, stride, budget_active, converged_ratio, variance_mean) = {
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        let samples_effective = u32_at(&ubo, 256)?;
        let stride = u32_at(&ubo, 296)?;
        let budget_active = u32_at(&ubo, 300)?;
        let update_max = u32_at(&ubo, 252)?;
        let mut converged = 0u32;
        let mut variance_sum = 0.0f64;
        let mut checked = 0u32;
        for i in 0..64u32 {
            let mut statistics = [0u8; PATCH_STATISTICS_SIZE as usize];
            tables
                .patch_statistics()
                .read(u64::from(i) * PATCH_STATISTICS_SIZE, &mut statistics)
                .map_err(internal_err("patch 统计读失败"))?;
            let update_count = u32_at(&statistics, 40)?;
            if update_count == 0 {
                continue;
            }
            checked += 1;
            if update_count >= update_max {
                converged += 1;
            }
            // 方差取 rgb 三通道均值
            let v =
                (f32_at(&statistics, 16)? + f32_at(&statistics, 20)? + f32_at(&statistics, 24)?)
                    / 3.0;
            variance_sum += f64::from(v);
        }
        let converged_ratio = if checked == 0 {
            0.0
        } else {
            f64::from(converged) / f64::from(checked)
        };
        let variance_mean = if checked == 0 {
            0.0
        } else {
            variance_sum / f64::from(checked)
        };
        (
            samples_effective,
            stride,
            budget_active,
            converged_ratio,
            variance_mean,
        )
    };
    let cell_count = config.cell_count().max(1);
    Ok(json!({
        "enabled": settings.enabled,
        "supported": true,
        "settings": settings_json(&settings),
        "slot": slot,
        "pool": {
            "active": active,
            "watermark": watermark,
            "capacity": config.capacity,
            "ring_start": ring_start,
            "ring_count": ring_count,
            "flip": ring[RING_FLIP],
        },
        // 本槽最近一次完整帧的计数（两帧内新鲜；GI 关的帧全 0）
        "counters": {
            "alloc": counters[COUNTER_ALLOC],
            "failed": counters[COUNTER_FAILED],
            "freed": counters[COUNTER_FREED],
            "defrag_moved": counters[COUNTER_DEFRAG_MOVED],
            "view_holes": counters[COUNTER_VIEW_HOLES],
            "view_deviation": counters[COUNTER_VIEW_DEVIATION],
            "view_covered": counters[COUNTER_VIEW_COVERED],
        },
        // 9.6 估计读数（估计关的帧全 0）：判定线要"射线数 / 命中数 / 参与 patch 数"
        "estimation": {
            "rays": counters[COUNTER_EST_RAYS],
            "hits": counters[COUNTER_EST_HITS],
            "bounce_requests": counters[COUNTER_EST_BOUNCE_REQUESTS],
            // 9.6.3 多跳自查询：命中缓存 / 落空（落空率就是桶错位＋覆盖不足的读数代理）
            "bounce_hits": counters[COUNTER_EST_BOUNCE_HITS],
            "bounce_misses": counters[COUNTER_EST_BOUNCE_MISSES],
            // 9.6.5 预算与收敛（样本档是预算换算后的实际值，不是设置值）
            "samples_effective": samples_effective,
            "stride": stride,
            "budget_active": budget_active,
            "converged_ratio": converged_ratio,
            "variance_mean": variance_mean,
            "patches": counters[COUNTER_EST_PATCHES],
            "samples": counters[COUNTER_EST_SAMPLES],
        },
        // 9.6.2 自发光三角形条数（超过容量 = 已截断，读数是超限值）
        "emissive_triangles": counters[COUNTER_EMISSIVE_TRIANGLES],
        // 9.6.2 光源侧采样清单：活灯数 / 本帧条数 / 命中有效的条数
        "light_samples": {
            "lights": light_count,
            "count": light_sample_total,
            "valid": light_sample_valid,
        },
        "patches": active,
        "cells": config.cell_count(),
        "cell_occupancy": f64::from(active) / cell_count as f64,
        "memory": {
            "cell_bytes": memory.cell_bytes,
            "patch_bytes": memory.patch_bytes,
            "misc_bytes": memory.misc_bytes,
            "total_bytes": memory.total(),
        },
        "gi_ms": gi_ms,
    }))
}

/// `gi_dump` 参数：`path` 必填；`slot` 可选（帧槽 0/1）。
#[derive(serde::Deserialize)]
struct GiDumpParams {
    path: String,
    #[serde(default)]
    slot: Option<u32>,
}

/// # Errors
/// 视图关/参数越界/LDR 读回或写文件失败。
#[system]
pub(crate) fn gi_dump(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GiDumpParams =
        crate::remote::common::params::parse_params(params, "params 缺失：需要 path（写 .ppm）")?;
    let settings = world
        .get_resource::<GiSettings>()
        .map_or_else(GiSettings::default, |s| *s);
    if !settings.enabled || settings.view == 0 {
        return Err(invalid_params(
            "patch 调试视图当前关闭（gi_set {\"enabled\":true,\"view\":N} 后再 dump）——没有视图可读",
        ));
    }
    let slot = crate::remote::census::frame_dump::check_slot(p.slot.unwrap_or(0) as usize)?;
    crate::remote::census::frame_dump::dump_ldr_frame(world, &p.path, slot)
}

/// `gi_census` 参数：抽检前 N 条 ring 槽（缺省 8，上限 64）。
#[derive(serde::Deserialize, Default)]
struct GiCensusParams {
    #[serde(default)]
    patches: Option<u32>,
}

/// `ash_renderer/gi_census`：cell↔patch 双向一致、patch 位置落在所属体素内、法线落在
/// 所属桶内。诊断路径，先 `device_wait_idle` 取一致快照（低频）。
///
/// # Errors
/// GI 关 / 资源缺席 / 读回失败。
#[system]
pub(crate) fn gi_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GiCensusParams = parse_params_or_default(params)?;
    let settings = world
        .get_resource::<GiSettings>()
        .map_or_else(GiSettings::default, |s| *s);
    if !settings.enabled {
        return Ok(json!({ "enabled": false }));
    }
    {
        let ctx = res::<Context>(world)?;
        unsafe { ctx.device.device_wait_idle() }.map_err(internal_err("GI 对账前排空失败"))?;
    }
    let slot = res::<FramePool>(world)?.current_index();
    let config = res::<SurfaceCacheTables>(world)?.config();
    let n = p.patches.unwrap_or(8).min(64);
    let res_value = config.res;
    let (ring, ubo) = {
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let ring = words(&read_buffer(
            tables.ring(),
            RING_CONFIG_WORDS * 4,
            "GI ring 配置读失败",
        )?);
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        (ring, ubo)
    };
    let flip = ring[RING_FLIP] as usize;
    let start = ring[RING_CONFIGS + flip * 4];
    let count = ring[RING_CONFIGS + flip * 4 + 1];
    let checked = n.min(count);
    let mut rows = Vec::with_capacity(checked as usize);
    let mut consistent = 0u32;
    let mut dead = 0u32;
    let mut dangling = 0u32;
    let mut in_voxel = 0u32;
    let mut bucket_ok = 0u32;
    let mut tables = res_mut::<SurfaceCacheTables>(world)?;
    for i in 0..checked {
        let ring_index = (start + i) % config.capacity.max(1);
        let mut raw = [0u8; 4];
        tables
            .patch_cell()
            .read(u64::from(ring_index) * 4, &mut raw)
            .map_err(internal_err("patch→cell 读失败"))?;
        let cell = u32::from_le_bytes(raw);
        if cell == INVALID_PATCH {
            dead += 1;
            continue;
        }
        let mut geometry = [0u8; 32];
        tables
            .patch_geometry()
            .read(u64::from(ring_index) * 32, &mut geometry)
            .map_err(internal_err("patch 几何读失败"))?;
        let mut statistics = [0u8; 48];
        tables
            .patch_statistics()
            .read(u64::from(ring_index) * 48, &mut statistics)
            .map_err(internal_err("patch 统计读失败"))?;
        let mut raw_cell = [0u8; 4];
        tables
            .cell_patch()
            .read(u64::from(cell) * 4, &mut raw_cell)
            .map_err(internal_err("cell→patch 读失败"))?;
        let mapped = u32::from_le_bytes(raw_cell);
        let mutual = mapped == ring_index;
        if mutual {
            consistent += 1;
        } else {
            dangling += 1;
        }
        let cascade = gi_cell_cascade(cell, res_value);
        let storage = gi_cell_storage(cell, res_value);
        let bucket = gi_cell_bucket(cell);
        let offset = if (cascade as usize) < crate::render::MAX_CASCADES {
            let base = 96 + cascade as usize * 16;
            bevy::math::IVec3::new(
                u32_at(&ubo, base)? as i32,
                u32_at(&ubo, base + 4)? as i32,
                u32_at(&ubo, base + 8)? as i32,
            )
        } else {
            bevy::math::IVec3::ZERO
        };
        let voxel = gi_voxel_size(config.voxel_min, cascade);
        let center = gi_volume_world_center(
            gi_storage_to_volume(storage, offset, res_value as i32),
            offset,
            voxel,
            res_value as i32,
        );
        let pos = Vec3::new(
            f32_at(&geometry, 0)?,
            f32_at(&geometry, 4)?,
            f32_at(&geometry, 8)?,
        );
        let normal = [
            f32_at(&geometry, 16)?,
            f32_at(&geometry, 20)?,
            f32_at(&geometry, 24)?,
        ];
        let distance = (pos - center).length();
        let within = distance <= voxel;
        if within {
            in_voxel += 1;
        }
        let normal_bucket = gi_direction_bucket(normal);
        let normal_ok = normal_bucket == bucket;
        if normal_ok {
            bucket_ok += 1;
        }
        rows.push(json!({
            "ring": ring_index,
            "cell": cell,
            "cascade": cascade,
            "storage": [storage.x, storage.y, storage.z],
            "bucket": bucket,
            "mapped": if mapped == INVALID_PATCH { json!(null) } else { json!(mapped) },
            "mutual": mutual,
            "position": [pos.x, pos.y, pos.z],
            "distance_to_voxel_center": distance,
            "voxel": voxel,
            "in_voxel": within,
            "normal_bucket": normal_bucket,
            "bucket_ok": normal_ok,
            "heartbeat": u32_at(&statistics, 44)?,
            "state": u32_at(&statistics, 32)?,
        }));
    }
    Ok(json!({
        "enabled": true,
        "slot": slot,
        "checked": checked,
        "ring_count": count,
        "summary": {
            "consistent": consistent,
            "dangling": dangling,
            "dead": dead,
            "in_voxel": in_voxel,
            "bucket_ok": bucket_ok,
        },
        "rows": rows,
    }))
}

/// `gi_estimate_probe` 参数：`patch`（池槽号，缺省 0）或 `ring`（ring 内序号，缺省不用）。
#[derive(serde::Deserialize, Default)]
struct GiEstimateProbeParams {
    #[serde(default)]
    patch: Option<u32>,
    #[serde(default)]
    ring: Option<u32>,
}

/// `ash_renderer/gi_estimate_probe`（9.6）：单条 patch 的几何/统计/两份辐照度对账。
///
/// 判定线要"单点对账"（多跳色渗、自发光 A/B、收敛），靠视图只能看颜色，所以这里回读数。
/// 诊断路径：先 `device_wait_idle` 取一致快照（人工低频调用，不在每帧链上）。
/// 两份辐照度都给出 + `parity`：本帧写的是 `patch_irradiance{parity}`，另一份是上一帧。
///
/// # Errors
/// GI 关 / 资源缺席 / 参数越界 / 读回失败。
#[system]
pub(crate) fn gi_estimate_probe(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GiEstimateProbeParams = parse_params_or_default(params)?;
    let settings = world
        .get_resource::<GiSettings>()
        .map_or_else(GiSettings::default, |s| *s);
    if !settings.enabled {
        return Ok(json!({ "enabled": false }));
    }
    {
        let ctx = res::<Context>(world)?;
        unsafe { ctx.device.device_wait_idle() }.map_err(internal_err("估计对账前排空失败"))?;
    }
    let slot = res::<FramePool>(world)?.current_index();
    let config = res::<SurfaceCacheTables>(world)?.config();
    let patch = if let Some(ring_index) = p.ring {
        let ring = {
            let mut tables = res_mut::<SurfaceCacheTables>(world)?;
            words(&read_buffer(
                tables.ring(),
                RING_CONFIG_WORDS * 4,
                "GI ring 配置读失败",
            )?)
        };
        let flip = ring[RING_FLIP] as usize;
        let count = ring[RING_CONFIGS + flip * 4 + 1];
        if ring_index >= count {
            return Err(invalid_params(format!(
                "ring 下标越界（本帧 ring 里 {count} 条）"
            )));
        }
        let start = ring[RING_CONFIGS + flip * 4];
        (start + ring_index) % config.capacity.max(1)
    } else {
        p.patch.unwrap_or(0)
    };
    if patch >= config.capacity {
        return Err(invalid_params(format!(
            "patch 越界（池容量 {}）",
            config.capacity
        )));
    }
    let (cell, geometry, statistics, irradiance0, irradiance1, parity) = {
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let mut raw_cell = [0u8; 4];
        tables
            .patch_cell()
            .read(u64::from(patch) * 4, &mut raw_cell)
            .map_err(internal_err("patch→cell 读失败"))?;
        let mut geometry = [0u8; PATCH_GEOMETRY_SIZE as usize];
        tables
            .patch_geometry()
            .read(u64::from(patch) * PATCH_GEOMETRY_SIZE, &mut geometry)
            .map_err(internal_err("patch 几何读失败"))?;
        let mut statistics = [0u8; PATCH_STATISTICS_SIZE as usize];
        tables
            .patch_statistics()
            .read(u64::from(patch) * PATCH_STATISTICS_SIZE, &mut statistics)
            .map_err(internal_err("patch 统计读失败"))?;
        let mut irradiance0 = [0u8; PATCH_IRRADIANCE_SIZE as usize];
        tables
            .patch_irradiance0()
            .read(u64::from(patch) * PATCH_IRRADIANCE_SIZE, &mut irradiance0)
            .map_err(internal_err("patch 辐照度 0 读失败"))?;
        let mut irradiance1 = [0u8; PATCH_IRRADIANCE_SIZE as usize];
        tables
            .patch_irradiance1()
            .read(u64::from(patch) * PATCH_IRRADIANCE_SIZE, &mut irradiance1)
            .map_err(internal_err("patch 辐照度 1 读失败"))?;
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        (
            u32::from_le_bytes(raw_cell),
            geometry,
            statistics,
            irradiance0,
            irradiance1,
            u32_at(&ubo, 240)? & 1,
        )
    };
    let irradiance_json = |raw: &[u8; PATCH_IRRADIANCE_SIZE as usize]| -> BrpResult {
        Ok(json!({
            "l0": [f32_at(raw, 0)?, f32_at(raw, 4)?, f32_at(raw, 8)?],
            "l1x": [f32_at(raw, 16)?, f32_at(raw, 20)?, f32_at(raw, 24)?],
            "l1y": [f32_at(raw, 32)?, f32_at(raw, 36)?, f32_at(raw, 40)?],
            "l1z": [f32_at(raw, 48)?, f32_at(raw, 52)?, f32_at(raw, 56)?],
        }))
    };
    Ok(json!({
        "enabled": true,
        "patch": patch,
        "alive": cell != INVALID_PATCH,
        "cell": if cell == INVALID_PATCH { json!(null) } else { json!(cell) },
        "geometry": {
            "position": [f32_at(&geometry, 0)?, f32_at(&geometry, 4)?, f32_at(&geometry, 8)?],
            "normal": [f32_at(&geometry, 16)?, f32_at(&geometry, 20)?, f32_at(&geometry, 24)?],
        },
        "statistics": {
            "mean": [f32_at(&statistics, 0)?, f32_at(&statistics, 4)?, f32_at(&statistics, 8)?],
            "variance": [f32_at(&statistics, 16)?, f32_at(&statistics, 20)?, f32_at(&statistics, 24)?],
            "state": u32_at(&statistics, 32)?,
            "rank": u32_at(&statistics, 32)? & 0xFF,
            "request": u32_at(&statistics, 36)?,
            "update_count": u32_at(&statistics, 40)?,
            "heartbeat": u32_at(&statistics, 44)?,
        },
        // 两份都给出：a = 第 0 份、b = 第 1 份。估计每帧只写其中一份（`frame&1`），
        // 所以**必有一份与 `statistics.mean` 逐位相等**（那是最后一帧写入的 l0），
        // 另一份是更早一帧的结果。`parity` 是 UBO 里的提示值，可能比最后一帧早一拍
        //（UBO 按帧槽写、读的是当前槽），对账请以"与 mean 相等的那一份"为准。
        "parity_hint": parity,
        "irradiance_a": irradiance_json(&irradiance0)?,
        "irradiance_b": irradiance_json(&irradiance1)?,
    }))
}
