//! 表面缓存方法族（9.5）：`gi_set` 写开关/视图/视图形态/法线来源/整理波数与 Volume 规格
//! （规格只在关态改，改后重建表），`gi_stats` 读池水位/计数/显存/`gi_ms`，`gi_dump` 把
//! LDR 帧（含 patch 视图）拷回写 PPM，`gi_census` 做 cell↔patch 双向一致与位置/桶归属对账。
//! GI 关时 stats/dump/census 回 `enabled:false` 形态（不报错，脚本靠这个判分支）。
use crate::remote::common::{
    params::{internal_err, invalid_params, parse_params_or_default, res, res_mut},
    readback::{f16_at, f32_at, read_buffer, u32_at, words},
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
use ash_macros::system;
use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};
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
    if p.view.is_some_and(|v| v > 15) {
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
/// `scgi_stats` 参数：stride 缺省 8；roi = [x, y, width, height]，缺省全图。
#[derive(serde::Deserialize, Default)]
struct ScgiStatsParams {
    #[serde(default)]
    stride: Option<u32>,
    #[serde(default)]
    roi: Option<[u32; 4]>,
}
/// `ash_renderer/scgi_stats`：读回 9.7 的全分辨率辐照度，报覆盖率与 L0 量级。
///
/// 判定线②（常量天空下 `E = πL` 的精确锚）与"注入是不是把画面压暗了"的归因都靠它：
/// 读的是 `scgi_irradiance` 的原始 RGBA16F 值（**物理单位**，不是 LDR），`a` 是覆盖率。
/// 按 `stride` 稀疏采样（缺省 8），一次读回的 CPU 代价与上报体积都可控。
///
/// # Errors
/// GI 关、目标未建或读回失败。
#[system]
pub(crate) fn scgi_stats(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ScgiStatsParams = crate::remote::common::params::parse_params_or_default(params)?;
    let stride = p.stride.unwrap_or(8).clamp(1, 64);
    let settings = world
        .get_resource::<GiSettings>()
        .map_or_else(GiSettings::default, |s| *s);
    if !settings.enabled {
        return Ok(json!({ "enabled": false }));
    }
    let Some(ctx) = world.get_resource::<Context>() else {
        return Err(internal_err("Context 缺席——初始化链未插入")(
            "scgi_stats 需要 Context",
        ));
    };
    let Some(scgi) = world.get_resource::<crate::render::ScgiTargets>() else {
        return Err(invalid_params("SCGI 目标资源缺席——初始化链未插入"));
    };
    let extent = scgi.full_extent();
    if extent.width == 0 || extent.height == 0 {
        return Err(invalid_params("SCGI 目标还没建（尺寸 0）"));
    }
    let bytes = scgi
        .readback_irradiance(ctx)
        .map_err(internal_err("SCGI 辐照度读回失败"))?;
    let width = extent.width as usize;
    let [x, y, w, h] = p.roi.unwrap_or([0, 0, extent.width, extent.height]);
    if w == 0 || h == 0 || x >= extent.width || y >= extent.height {
        return Err(invalid_params(
            "roi 必须是图内非空矩形 [x, y, width, height]",
        ));
    }
    let end_x = x.saturating_add(w).min(extent.width) as usize;
    let end_y = y.saturating_add(h).min(extent.height) as usize;
    let mut covered = 0u64;
    let mut sampled = 0u64;
    let mut sum_l0 = [0.0f64; 3];
    let mut sum_covered_l0 = [0.0f64; 3];
    let mut max_l0 = 0.0f32;
    let mut max_alpha = 0.0f32;
    let mut sum_alpha = 0.0f64;
    let mut non_finite = 0u64;
    let mut negative = 0u64;
    let step = stride as usize;
    for y in (y as usize..end_y).step_by(step) {
        for x in (x as usize..end_x).step_by(step) {
            let base = (y * width + x) * 8;
            if base + 8 > bytes.len() {
                continue;
            }
            let channel = |i: usize| {
                crate::render::half_to_f32(u16::from_le_bytes([
                    bytes[base + i * 2],
                    bytes[base + i * 2 + 1],
                ]))
            };
            let rgb = [channel(0), channel(1), channel(2)];
            let alpha = channel(3);
            if !alpha.is_finite() || rgb.iter().any(|v| !v.is_finite()) {
                non_finite += 1;
                continue;
            }
            if rgb.iter().any(|v| *v < 0.0) {
                negative += 1;
            }
            sampled += 1;
            sum_alpha += f64::from(alpha);
            max_alpha = max_alpha.max(alpha);
            for (c, v) in rgb.iter().enumerate() {
                sum_l0[c] += f64::from(*v);
            }
            if alpha > 0.001 {
                covered += 1;
                for (c, v) in rgb.iter().enumerate() {
                    sum_covered_l0[c] += f64::from(*v);
                }
                max_l0 = max_l0.max(rgb[0].max(rgb[1]).max(rgb[2]));
            }
        }
    }
    if sampled == 0 {
        return Err(internal_err("SCGI 辐照度读回为空")(
            "scgi_stats 采样数为 0",
        ));
    }
    let n = sampled as f64;
    let covered_f = covered.max(1) as f64;
    Ok(json!({
        "enabled": true,
        "extent": { "width": extent.width, "height": extent.height },
        "stride": stride,
        "sampled": sampled,
        "roi": [x, y, end_x as u32 - x, end_y as u32 - y],
        "non_finite": non_finite,
        "negative": negative,
        "coverage": covered as f64 / n,
        "alpha": { "mean": sum_alpha / n, "max": max_alpha },
        "l0_all_mean": [
            sum_l0[0] / n,
            sum_l0[1] / n,
            sum_l0[2] / n,
        ],
        "l0_covered_mean": [
            sum_covered_l0[0] / covered_f,
            sum_covered_l0[1] / covered_f,
            sum_covered_l0[2] / covered_f,
        ],
        "l0_covered_max": max_l0,
        "scgi": settings.scgi,
        "ibl_intensity_note": "l0_* 是物理单位辐照度；常量天空 L=1 时理论值应 ≈ π",
    }))
}

/// `scgi_low_probe` 参数：全分辨率像素坐标（读它所属的 1/4 块）。
#[derive(serde::Deserialize, Default)]
struct ScgiLowProbeParams {
    #[serde(default)]
    pixel: Option<[u32; 2]>,
}

/// `ash_renderer/scgi_low_probe`：读回某像素所属 1/4 块的 L0/L1 原始内容（双缓冲各一份）。
///
/// 9.7.4 的覆盖率不稳定性只能在这一层定位：全分辨率图的 `a` 已被去噪与上采样按几何权重
/// 重新归一，看不出"是查表没覆盖还是上采样稀释"。这里直接读 `scgi_l0`/`scgi_l1` 的块值，
/// 把 `l0.w`（覆盖率）、RGBL1 与 `l1.w`（guide 相位）原样报出来。
///
/// # Errors
/// GI 关、目标未建或读回失败。
#[system]
pub(crate) fn scgi_low_probe(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ScgiLowProbeParams = parse_params_or_default(params)?;
    let pixel = p
        .pixel
        .ok_or_else(|| invalid_params("scgi_low_probe 需要 pixel = [x, y]"))?;
    let Some(ctx) = world.get_resource::<Context>() else {
        return Err(internal_err("Context 缺席——初始化链未插入")(
            "scgi_low_probe 需要 Context",
        ));
    };
    let Some(scgi) = world.get_resource::<crate::render::ScgiTargets>() else {
        return Err(invalid_params("SCGI 目标资源缺席——初始化链未插入"));
    };
    let low = scgi.low_extent();
    let full = scgi.full_extent();
    if low.width == 0 || low.height == 0 {
        return Err(invalid_params("SCGI 目标还没建（尺寸 0）"));
    }
    if pixel[0] >= full.width || pixel[1] >= full.height {
        return Err(invalid_params("pixel 超出全分辨率画面"));
    }
    let block = ash::vk::Offset2D {
        x: (pixel[0] / 4).min(low.width - 1) as i32,
        y: (pixel[1] / 4).min(low.height - 1) as i32,
    };
    let one = ash::vk::Extent2D {
        width: 1,
        height: 1,
    };
    let read = |image: ash::vk::Image, bytes_per_texel: u64| {
        crate::render::readback_image_region(
            &ctx,
            image,
            ash::vk::ImageAspectFlags::COLOR,
            ash::vk::ImageLayout::GENERAL,
            block,
            one,
            bytes_per_texel,
        )
        .map_err(internal_err("1/4 目标读回失败"))
    };
    let mut buffers = Vec::with_capacity(2);
    for index in 0..2 {
        let l0_raw = read(scgi.low_l0_image(index), 8)?;
        let l1_raw = read(scgi.low_l1_image(index), 4)?;
        let mut rgba = [0.0f32; 4];
        for (i, slot) in rgba.iter_mut().enumerate() {
            *slot = f16_at(&l0_raw, i * 2)?;
        }
        buffers.push(json!({
            "l0": [rgba[0], rgba[1], rgba[2]],
            "coverage": rgba[3],
            "l1": [l1_raw[0], l1_raw[1], l1_raw[2], l1_raw[3]],
        }));
    }
    Ok(json!({
        "pixel": pixel,
        "block": [block.x, block.y],
        "low_extent": { "width": low.width, "height": low.height },
        "parity": res::<SurfaceCacheTables>(world)?
            .submitted_gi_frame()
            .map(|f| f.frame_index & 1),
        "buffers": buffers,
    }))
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
    let Some(slot) = res::<SurfaceCacheTables>(world)?.submitted_gi_slot() else {
        return Ok(json!({"enabled": true, "reason": "not_ready"}));
    };
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

struct PixelInput {
    surface: Option<(Vec3, Vec3)>,
    value: Value,
}

fn pixel_input(
    world: &mut World,
    slot: usize,
    pixel: [u32; 2],
    normal_source: u32,
) -> Result<PixelInput, bevy::remote::BrpError> {
    use crate::render::{
        gi_flat_normal, gi_world_from_depth, readback_image_region, FrameUniformTable,
        GBufferTargets, ScgiTargets, SmaaTargets, FRAME_UBO_SIZE,
    };
    use ash::vk;

    let frame = {
        let mut table = res_mut::<FrameUniformTable>(world)?;
        read_buffer(
            table.ubo(slot),
            FRAME_UBO_SIZE as usize,
            "像素对账帧 UBO 读失败",
        )?
    };
    let ctx = res::<Context>(world)?;
    let gbuffer = res::<GBufferTargets>(world)?;
    let scgi = res::<ScgiTargets>(world)?;
    let extent = res::<SmaaTargets>(world)?.ldr_extent();
    if pixel[0] >= extent.width || pixel[1] >= extent.height {
        return Err(invalid_params("pixel 越出全分辨率目标"));
    }
    let screen_current = u32_at(&frame, 336)? != 0;
    if screen_current && (!scgi.initialized() || scgi.full_extent() != extent) {
        return Ok(PixelInput {
            surface: None,
            value: json!({"pixel": pixel, "reason": "screen_not_ready"}),
        });
    }
    let origin = [pixel[0].saturating_sub(1), pixel[1].saturating_sub(1)];
    let last = [
        pixel[0].saturating_add(1).min(extent.width - 1),
        pixel[1].saturating_add(1).min(extent.height - 1),
    ];
    let region = vk::Extent2D {
        width: last[0] - origin[0] + 1,
        height: last[1] - origin[1] + 1,
    };
    let depth = readback_image_region(
        &ctx,
        res::<FramePool>(world)?.depth_image(slot),
        vk::ImageAspectFlags::DEPTH,
        vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        vk::Offset2D {
            x: origin[0] as i32,
            y: origin[1] as i32,
        },
        region,
        4,
    )
    .map_err(internal_err("深度 stencil 回读失败"))?;
    let read_color = |image, layout, bytes| {
        readback_image_region(
            &ctx,
            image,
            vk::ImageAspectFlags::COLOR,
            layout,
            vk::Offset2D {
                x: pixel[0] as i32,
                y: pixel[1] as i32,
            },
            vk::Extent2D {
                width: 1,
                height: 1,
            },
            bytes,
        )
        .map_err(internal_err("像素颜色目标回读失败"))
    };
    let sampled = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    let normal_bytes = read_color(gbuffer.normal_image(slot), sampled, 8)?;
    let albedo = read_color(gbuffer.albedo_image(slot), sampled, 4)?;
    let material = read_color(gbuffer.matparam_image(slot), sampled, 4)?;
    let hdr = read_color(gbuffer.hdr_image(slot), sampled, 8)?;
    let irradiance = if screen_current {
        Some(read_color(
            scgi.irradiance_image(),
            vk::ImageLayout::GENERAL,
            8,
        )?)
    } else {
        None
    };
    let rgba_half = |raw: &[u8]| -> Result<[f32; 4], bevy::remote::BrpError> {
        Ok([
            f16_at(raw, 0)?,
            f16_at(raw, 2)?,
            f16_at(raw, 4)?,
            f16_at(raw, 6)?,
        ])
    };
    let size = Vec2::new(extent.width as f32, extent.height as f32);
    let uv = (Vec2::new(pixel[0] as f32, pixel[1] as f32) + Vec2::splat(0.5)) / size;
    let mut inv = [0.0; 16];
    let mut view_proj = [0.0; 16];
    for i in 0..16 {
        inv[i] = f32_at(&frame, 128 + i * 4)?;
        view_proj[i] = f32_at(&frame, i * 4)?;
    }
    let inv = Mat4::from_cols_array(&inv);
    let camera = Vec3::new(
        f32_at(&frame, 192)?,
        f32_at(&frame, 196)?,
        f32_at(&frame, 200)?,
    );
    let light = Vec3::new(
        f32_at(&frame, 64)?,
        f32_at(&frame, 68)?,
        f32_at(&frame, 72)?,
    );
    let neighbors = [
        pixel,
        [pixel[0].saturating_sub(1), pixel[1]],
        [pixel[0].saturating_add(1).min(extent.width - 1), pixel[1]],
        [pixel[0], pixel[1].saturating_add(1).min(extent.height - 1)],
        [pixel[0], pixel[1].saturating_sub(1)],
    ];
    let mut depths = [0.0; 5];
    let mut stencil = Vec::with_capacity(5);
    for (i, xy) in neighbors.into_iter().enumerate() {
        let index = (xy[1] - origin[1]) * region.width + xy[0] - origin[0];
        depths[i] = f32_at(&depth, index as usize * 4)?;
        let neighbor_uv = (Vec2::new(xy[0] as f32, xy[1] as f32) + Vec2::splat(0.5)) / size;
        let position =
            (depths[i] > 0.0).then(|| gi_world_from_depth(neighbor_uv, depths[i], inv).to_array());
        stencil.push(json!({"pixel": xy, "depth": depths[i], "position": position}));
    }
    let mut value = json!({
        "pixel": pixel, "ubo_slot": slot, "extent": [extent.width, extent.height],
        "view_proj": view_proj, "camera": camera.to_array(), "sun_dir": light.to_array(),
        "depth_stencil": stencil, "normal_source": normal_source,
        "albedo": [
            f32::from(albedo[0]) / 255.0,
            f32::from(albedo[1]) / 255.0,
            f32::from(albedo[2]) / 255.0,
        ],
        "material": {
            "metallic": f32::from(material[0]) / 255.0,
            "ao": f32::from(material[1]) / 255.0,
            "roughness": f32::from(material[2]) / 255.0,
            "reflectance": f32::from(material[3]) / 255.0,
        },
        "hdr": rgba_half(&hdr)?, "hdr_color_space": "linear_srgb_hdr_target",
        "screen_current": screen_current,
        "screen_irradiance": irradiance.as_ref().map(|bytes| rgba_half(bytes)).transpose()?,
        "frame_mode": u32_at(&frame, 112)?,
    });
    if depths[0] <= 0.0 {
        return Ok(PixelInput {
            surface: None,
            value,
        });
    }
    let position = gi_world_from_depth(uv, depths[0], inv);
    let encoded_normal = rgba_half(&normal_bytes)?;
    let mut shading =
        Vec3::new(encoded_normal[0], encoded_normal[1], encoded_normal[2]).normalize_or_zero();
    if shading.dot(camera - position) < 0.0 {
        shading = -shading;
    }
    let flat = gi_flat_normal(uv, Vec2::ONE / size, depths, inv, camera);
    let selected = if normal_source == 0 { flat } else { shading };
    value["position"] = json!(position.to_array());
    value["flat_normal"] = json!(flat.to_array());
    value["shading_normal"] = json!(shading.to_array());
    value["selected_normal"] = json!(selected.to_array());
    value["no_l"] = json!(selected.dot(light).max(0.0));
    let valid = position.is_finite() && selected.is_finite() && selected.length_squared() >= 1e-8;
    Ok(PixelInput {
        surface: valid.then_some((position, selected)),
        value,
    })
}
/// `gi_estimate_probe` 参数：`pixel`（全分辨率）或 `position`+`normal`，或 `patch`/`ring`。
#[derive(serde::Deserialize, Default)]
struct GiEstimateProbeParams {
    #[serde(default)]
    pixel: Option<[u32; 2]>,
    #[serde(default)]
    patch: Option<u32>,
    #[serde(default)]
    ring: Option<u32>,
    #[serde(default)]
    position: Option<[f32; 3]>,
    #[serde(default)]
    normal: Option<[f32; 3]>,
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
    let mut p: GiEstimateProbeParams = parse_params_or_default(params)?;
    if p.pixel.is_some()
        && (p.patch.is_some() || p.ring.is_some() || p.position.is_some() || p.normal.is_some())
    {
        return Err(invalid_params("pixel 与 patch/ring/position/normal 互斥"));
    }
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
    // Read the actual submission, not the next reusable slot or live UI settings.
    let Some(submitted) = res::<SurfaceCacheTables>(world)?.submitted_gi_frame() else {
        return Ok(json!({"enabled": true, "alive": false, "reason": "not_ready"}));
    };
    let slot = submitted.slot;
    let submission = json!({"slot": slot, "gi_frame_index": submitted.frame_index,
        "normal_source": submitted.normal_source, "estimation": submitted.has_estimation,
        "scgi": submitted.has_scgi, "extent": submitted.extent,
        "requested_normal_source": settings.normal_source});
    let mut input = json!(null);
    if let Some(pixel) = p.pixel {
        if !settings.estimation || !submitted.has_estimation {
            return Ok(
                json!({"enabled": true, "alive": false, "reason": "producer_stopped",
                "submission": submission}),
            );
        }
        let extent = res::<crate::render::SmaaTargets>(world)?.ldr_extent();
        if submitted.extent != [extent.width, extent.height] {
            return Ok(
                json!({"enabled": true, "alive": false, "reason": "frame_extent_changed",
                "submission": submission}),
            );
        }
        let PixelInput { surface, value } =
            pixel_input(world, slot, pixel, submitted.normal_source)?;
        input = value;
        input["submission"] = submission.clone();
        let Some((position, normal)) = surface else {
            return Ok(
                json!({"enabled": true, "alive": false, "reason": "invalid_surface", "input": input}),
            );
        };
        p.position = Some(position.to_array());
        p.normal = Some(normal.to_array());
    }
    let config = res::<SurfaceCacheTables>(world)?.config();
    let frame_cursor = res::<FramePool>(world)?.current_index();
    let mut lookup = json!(null);
    let patch = if let Some(position) = p.position {
        let normal = Vec3::from_array(
            p.normal
                .ok_or_else(|| invalid_params("position 查表需要 normal"))?,
        );
        let position = Vec3::from_array(position);
        if !position.is_finite() || !normal.is_finite() || normal.length_squared() < 1e-8 {
            return Err(invalid_params("position/normal 必须有限，normal 不得为零"));
        }
        let mut tables = res_mut::<SurfaceCacheTables>(world)?;
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        let center = Vec3::new(f32_at(&ubo, 64)?, f32_at(&ubo, 68)?, f32_at(&ubo, 72)?);
        lookup = json!({"ubo_slot": slot, "next_frame_slot": frame_cursor,
            "frame_slots": crate::render::MAX_FRAMES_IN_FLIGHT,
            "gi_frame_index": u32_at(&ubo, 240)?,
            "center": center.to_array(), "voxel_min": config.voxel_min,
            "res": config.res, "cascades": config.cascades});
        let Some(cascade) = crate::render::gi_resolve_cascade(
            center,
            position,
            config.voxel_min,
            config.res,
            config.cascades,
        ) else {
            return Ok(
                json!({"enabled": true, "alive": false, "reason": "outside_volume", "lookup": lookup, "input": input}),
            );
        };
        let base = 96 + cascade as usize * 16;
        let offset = IVec3::new(
            u32_at(&ubo, base)? as i32,
            u32_at(&ubo, base + 4)? as i32,
            u32_at(&ubo, base + 8)? as i32,
        );
        let voxel = gi_voxel_size(config.voxel_min, cascade);
        let pv = crate::render::gi_volume_position(position, voxel, offset, config.res as i32);
        let storage = crate::render::gi_volume_to_storage(pv, offset, config.res as i32);
        let cell = crate::render::gi_cell_index(
            cascade,
            storage,
            gi_direction_bucket(normal.normalize().to_array()),
            config.res,
        );
        let mut raw = [0u8; 4];
        tables
            .cell_patch()
            .read(u64::from(cell) * 4, &mut raw)
            .map_err(internal_err("cell→patch 读失败"))?;
        let patch = u32::from_le_bytes(raw);
        if patch == INVALID_PATCH {
            return Ok(
                json!({"enabled": true, "alive": false, "cell": cell, "reason": "cache_miss", "lookup": lookup, "input": input}),
            );
        }
        patch
    } else if p.normal.is_some() {
        return Err(invalid_params("normal 查表需要 position"));
    } else if let Some(ring_index) = p.ring {
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
    let (cell, geometry, statistics, irradiance0, irradiance1, spatial, temporal, frame_index) = {
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
        let mut spatial = [0u8; PATCH_IRRADIANCE_SIZE as usize];
        tables
            .filtered_irradiance0()
            .read(u64::from(patch) * PATCH_IRRADIANCE_SIZE, &mut spatial)
            .map_err(internal_err("patch 空间滤波读失败"))?;
        let mut temporal = [0u8; PATCH_IRRADIANCE_SIZE as usize];
        tables
            .filtered_irradiance1()
            .read(u64::from(patch) * PATCH_IRRADIANCE_SIZE, &mut temporal)
            .map_err(internal_err("patch 时域滤波读失败"))?;
        let ubo = read_buffer(tables.ubo(slot), GI_UBO_SIZE as usize, "GI UBO 读失败")?;
        (
            u32::from_le_bytes(raw_cell),
            geometry,
            statistics,
            irradiance0,
            irradiance1,
            spatial,
            temporal,
            u32_at(&ubo, 240)?,
        )
    };
    if frame_index != submitted.frame_index {
        return Err(internal_err("GI UBO 与提交快照帧号不一致")(
            format!("UBO={frame_index}, submitted={}", submitted.frame_index),
        ));
    }
    let normal = [
        f32_at(&geometry, 16)?,
        f32_at(&geometry, 20)?,
        f32_at(&geometry, 24)?,
    ];
    let irradiance_json = |raw: &[u8; PATCH_IRRADIANCE_SIZE as usize],
                           include_diagnostics: bool|
     -> BrpResult {
        let l0 = [f32_at(raw, 0)?, f32_at(raw, 4)?, f32_at(raw, 8)?];
        let l1x = [f32_at(raw, 16)?, f32_at(raw, 20)?, f32_at(raw, 24)?];
        let l1y = [f32_at(raw, 32)?, f32_at(raw, 36)?, f32_at(raw, 40)?];
        let l1z = [f32_at(raw, 48)?, f32_at(raw, 52)?, f32_at(raw, 56)?];
        let evaluated = [0, 1, 2].map(|i| {
            (l0[i] + l1x[i] * normal[0] + l1y[i] * normal[1] + l1z[i] * normal[2]).max(0.0)
        });
        let mut value = json!({
            "l0": l0, "l1x": l1x, "l1y": l1y, "l1z": l1z,
            "evaluated": evaluated,
            "evaluated_normal": normal,
            "reserved": [f32_at(raw, 12)?, f32_at(raw, 28)?, f32_at(raw, 44)?, f32_at(raw, 60)?],
        });
        if include_diagnostics {
            // 只有 raw 缓冲带未滤波的天空/命中拆分；滤波后的 b14/b15 的 .w 是帧戳。
            value["diag"] = json!({
                "sky_e": f32_at(raw, 12)?, "hit_e": f32_at(raw, 28)?,
                "sky_frac": f32_at(raw, 44)?, "sun_visibility": f32_at(raw, 60)?,
            });
        }
        Ok(value)
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
        // raw 两份是方差引导的长时结果；statistics.mean 是短时均值，不承诺逐位相等。
        // 版本来自最近实际 GI 提交，并已等待 GPU 完成；GI 帧号不是图形提交次数。
        "ubo_slot": slot,
        "gi_frame_index": frame_index,
        "lookup": lookup,
        "input": input,
        "submission": submission,
        "raw_current": submitted.has_estimation,
        "raw_diagnostic_layout": "sky_hit_fraction_sun_v2",
        "parity_hint": frame_index & 1,
        "irradiance_a": irradiance_json(&irradiance0, true)?,
        "irradiance_b": irradiance_json(&irradiance1, true)?,
        "irradiance_spatial": irradiance_json(&spatial, false)?,
        "irradiance_temporal": irradiance_json(&temporal, false)?,
    }))
}
