//! 环境光照方法族：`ibl_set`（开关/强度/漫反射编码，缺省字段不动）、`ibl_stats`
//!（设置、SH-L2 系数、两条自检误差、捕获账本、GlobalProbe 六方向摘要）与
//! `ibl_census`（cubemap/DFG/GlobalProbe 全量读回）。
//!
//! 读数允许滞后（BRP 不是正确性路径）：SH/stats/probe 缓冲在捕获帧由 compute 写，
//! 本方法用一次性拷贝 + fence 读回，取到的是"某个完整状态"（同 `hiz_census` 口径）。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    bucket_to_direction, direction_to_bucket, eval_probe_bucket, readback_buffer, Context,
    IblEnvironment, IblSettings, PerfLedger, IBL_CUBE_MIPS, IBL_MODE_PROBE16, IBL_MODE_SH_L2,
    IBL_PROBE_BUCKETS, IBL_PROBE_BUFFER_SIZE, IBL_PROBE_RESOLUTION, IBL_SH_BUFFER_SIZE,
    IBL_STATS_BUFFER_SIZE, IBL_STATS_FLOATS,
};

use crate::remote::common::params::{internal_err, invalid_params, parse_params, res, res_mut};

/// `ash_renderer/ibl_set`：环境光照开关、强度与漫反射编码。请求
/// `{"enabled": bool, "intensity": f32?, "diffuse_mode": "sh_l2"|"probe16"?}`，
/// 缺省字段不动；回显当前值。
#[system]
pub(crate) fn ibl_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> = parse_params(
        params,
        "params 缺失：需要 enabled/intensity/diffuse_mode 之一",
    )?;
    let mut ibl = res_mut::<IblSettings>(world)?;
    let mut echo = json!({});
    if let Some(v) = p.get("enabled") {
        let enabled = v
            .as_bool()
            .ok_or_else(|| invalid_params("enabled 非布尔"))?;
        ibl.enabled = enabled;
        echo["enabled"] = json!(enabled);
    }
    if let Some(v) = p.get("intensity") {
        let intensity = v
            .as_f64()
            .ok_or_else(|| invalid_params("intensity 非数值"))? as f32;
        if !intensity.is_finite() || intensity < 0.0 {
            return Err(invalid_params("intensity 须为非负有限值"));
        }
        ibl.intensity = intensity;
        echo["intensity"] = json!(intensity);
    }
    if let Some(v) = p.get("diffuse_mode") {
        let name = v
            .as_str()
            .ok_or_else(|| invalid_params("diffuse_mode 非字符串"))?;
        let mode = match name {
            "sh_l2" => IBL_MODE_SH_L2,
            "probe16" => IBL_MODE_PROBE16,
            other => {
                return Err(invalid_params(format!(
                    "diffuse_mode 只收 sh_l2/probe16，收到 {other:?}"
                )))
            }
        };
        ibl.mode = mode;
        echo["diffuse_mode"] = json!(name);
    }
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params(
            "无有效字段（enabled/intensity/diffuse_mode 任一）",
        ));
    }
    Ok(json!({ "applied": echo, "current": current(&ibl) }))
}

/// `ash_renderer/ibl_stats`：设置、SH-L2 系数、两条自检误差、捕获账本与 GlobalProbe
/// 六方向摘要。Vulkan 资源缺席（禁渲染宿主）时只回设置。
#[system]
pub(crate) fn ibl_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let settings = current(res::<IblSettings>(world)?);
    let (Some(ctx), Some(env)) = (
        world.get_resource::<Context>(),
        world.get_resource::<IblEnvironment>(),
    ) else {
        return Ok(json!({ "settings": settings, "gpu": "Vulkan 资源缺席" }));
    };

    // SH 系数：9 行 vec4（xyz = RGB，w 未用）
    let mut sh_bytes = vec![0u8; IBL_SH_BUFFER_SIZE as usize];
    readback_buffer(ctx, env.sh_buffer(), &mut sh_bytes)
        .map_err(internal_err("SH 缓冲读回失败"))?;
    let coefficients: Vec<[f32; 3]> = sh_bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|row| {
            let mut rgb = [0.0f32; 3];
            for (i, c) in rgb.iter_mut().enumerate() {
                *c = f32::from_le_bytes(row[i * 4..i * 4 + 4].try_into().unwrap_or([0; 4]));
            }
            rgb
        })
        .collect();

    // 自检：`[0]` = l=0 归一化、`[1..15]` = 14 个法线的求值对积分相对误差
    let mut stats_bytes = vec![0u8; IBL_STATS_BUFFER_SIZE as usize];
    readback_buffer(ctx, env.stats_buffer(), &mut stats_bytes)
        .map_err(internal_err("IBL 自检读回失败"))?;
    let stats: Vec<f32> = stats_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(IBL_STATS_FLOATS)
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    let normal_errors: Vec<f32> = stats.get(1..15).map(<[f32]>::to_vec).unwrap_or_default();
    let max_normal_error = normal_errors.iter().copied().fold(0.0f32, f32::max);

    // GlobalProbe 16 桶：读回 1KB（16×4 vec4），按 ±X/±Y/±Z 六个方向求值
    let mut probe_bytes = vec![0u8; IBL_PROBE_BUFFER_SIZE as usize];
    readback_buffer(ctx, env.probe_buffer(), &mut probe_bytes)
        .map_err(internal_err("GlobalProbe 读回失败"))?;
    let probe_floats: Vec<f32> = probe_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    let mut buckets = [[[0.0f32; 4]; 4]; IBL_PROBE_BUCKETS as usize];
    for (bucket, row) in buckets.iter_mut().enumerate() {
        for (line, value) in row.iter_mut().enumerate() {
            let base = (bucket * 4 + line) * 4;
            for (channel, slot) in value.iter_mut().enumerate() {
                *slot = probe_floats.get(base + channel).copied().unwrap_or(0.0);
            }
        }
    }
    let mut directions = serde_json::Map::new();
    for (name, direction) in [
        ("+x", [1.0f32, 0.0, 0.0]),
        ("-x", [-1.0, 0.0, 0.0]),
        ("+y", [0.0, 1.0, 0.0]),
        ("-y", [0.0, -1.0, 0.0]),
        ("+z", [0.0, 0.0, 1.0]),
        ("-z", [0.0, 0.0, -1.0]),
    ] {
        let index = direction_to_bucket(direction, IBL_PROBE_RESOLUTION);
        let e = eval_probe_bucket(direction, &buckets[index as usize]);
        directions.insert(name.to_string(), json!({ "bucket": index, "E": e }));
    }
    // 能量自检的唯一正确采样点：每桶在自己的**桶中心方向**求值（常量天空 ≈ πL）。
    // ±X/±Y/±Z 落在桶内非中心处，L1 的各向异性会让 E 偏 ±10%，不能当能量判据。
    let center_energy: Vec<[f32; 3]> = (0..IBL_PROBE_BUCKETS)
        .map(|index| {
            let center = bucket_to_direction(index, IBL_PROBE_RESOLUTION);
            eval_probe_bucket(center, &buckets[index as usize])
        })
        .collect();
    let probe16 = json!({
        "resolution": IBL_PROBE_RESOLUTION,
        "buckets": IBL_PROBE_BUCKETS,
        "directions": directions,
        "center_energy": center_energy,
    });

    let (capture_count, last_hash) = env.bake_state();
    // 捕获的 GPU 耗时（帧样本账）：普通帧的 IBL 时间戳区间只有 µs 级噪声，取
    // >0.1ms 的样本才算真捕帧；同时报整个窗口的最大值（最近一次捕获 = 窗口内最大那一档）。
    const CAPTURE_NOISE_MS: f32 = 0.1;
    let samples = world
        .get_resource::<PerfLedger>()
        .map(|perf| perf.snapshot())
        .unwrap_or_default();
    let capture_ms = samples
        .iter()
        .rev()
        .map(|s| s.gpu_ibl_capture_ms)
        .find(|ms| *ms > CAPTURE_NOISE_MS);
    let capture_window_max_ms = samples
        .iter()
        .map(|s| s.gpu_ibl_capture_ms)
        .fold(0.0f32, f32::max);
    let lighting_ms = world
        .get_resource::<PerfLedger>()
        .and_then(|perf| perf.percentile(0.5, |s| s.gpu_lighting_ms));

    Ok(json!({
        "settings": settings,
        "sh_l2": coefficients,
        "probe16": probe16,
        "self_check": {
            "valid": capture_count > 0,
            "l0_rel_error": stats.first().copied(),
            "normal_max_rel_error": max_normal_error,
            "normal_rel_errors": normal_errors,
        },
        "capture": {
            "count": capture_count,
            "last_hash": last_hash,
            "last_capture_gpu_ms": capture_ms,
            "window_max_gpu_ms": capture_window_max_ms,
        },
        "gpu_lighting_ms_p50": lighting_ms,
    }))
}

/// `ash_renderer/ibl_census` 参数：`mip` 可选（缺省 0 = 锐利源；1..8 = 预滤波级）；
/// `probe: true` 读回 GlobalProbe 16 桶；`lut: true` 读回 DFG LUT。
#[derive(serde::Deserialize)]
struct IblCensusParams {
    #[serde(default)]
    mip: Option<u32>,
    /// true = 读回 GlobalProbe 16 桶（16×4 个 float，全量取证）。
    #[serde(default)]
    probe: bool,
    /// true = 读回 DFG LUT 而不是 cubemap。
    #[serde(default)]
    lut: bool,
    /// 与 `lut: true` 搭配：回该行（y = perceptualRoughness，0..127）的 128 个 A/B。
    #[serde(default)]
    row: Option<u32>,
}

/// `ash_renderer/ibl_census`：读回环境 cubemap 某一级的六个面，报 min/max/mean、
/// 非有限值计数与中心像素（面方向与接缝的取证口）；`probe: true` 改回 GlobalProbe
/// 16 桶的 64 个 float（映射/能量取证）；`lut: true` 改读 DFG LUT 的
/// min/max/非有限计数，`lut + row` 回该行 128 个 A/B（DFG 形状的取证口——只看
/// min/max 看不出"掠射端整段钳 1"这类形状问题，见 9.2.8）。只进验证，不进绘制路径。
///
/// # Errors
/// mip 越界 / 资源缺席 / 读回失败。
#[system]
pub(crate) fn ibl_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: IblCensusParams = parse_params(params, "params 缺失：需要对象（mip 可选）")?;
    let mip = p.mip.unwrap_or(0);
    if mip >= IBL_CUBE_MIPS {
        return Err(invalid_params(format!(
            "mip {mip} 越界（环境 cubemap {IBL_CUBE_MIPS} 级）"
        )));
    }
    let (Some(ctx), Some(env)) = (
        world.get_resource::<Context>(),
        world.get_resource::<IblEnvironment>(),
    ) else {
        return Err(invalid_params("Vulkan 资源缺席——IBL 未初始化"));
    };
    if p.probe {
        let mut bytes = vec![0u8; IBL_PROBE_BUFFER_SIZE as usize];
        readback_buffer(ctx, env.probe_buffer(), &mut bytes)
            .map_err(internal_err("GlobalProbe 读回失败"))?;
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        // 桶中心方向：与 values 的桶序一一对应，映射取证时直接对照
        let centers: Vec<[f32; 3]> = (0..IBL_PROBE_BUCKETS)
            .map(|index| bucket_to_direction(index, IBL_PROBE_RESOLUTION))
            .collect();
        return Ok(json!({
            "probe": true,
            "buckets": IBL_PROBE_BUCKETS,
            "resolution": IBL_PROBE_RESOLUTION,
            "values": values,
            "centers": centers,
        }));
    }
    if p.lut {
        let rgba = env
            .readback_dfg(ctx)
            .map_err(internal_err("DFG LUT 读回失败"))?;
        // 单行取样：y = 粗糙度行，x = NoV，A/B 分开给（验收 DFG 形状用）
        if let Some(row) = p.row {
            // LUT 是方图：行宽 = sqrt(texel 数)
            let width = (rgba.len() / 4).isqrt();
            let row = (row as usize).min(width.saturating_sub(1));
            let base = row * width * 4;
            let mut a = Vec::with_capacity(width);
            let mut b = Vec::with_capacity(width);
            for texel in rgba[base..base + width * 4].as_chunks::<4>().0 {
                a.push(texel[0]);
                b.push(texel[1]);
            }
            return Ok(json!({ "lut": true, "row": row, "a": a, "b": b }));
        }
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut non_finite = 0u64;
        let mut bad_pixels = Vec::new();
        for (index, texel) in rgba.as_chunks::<4>().0.iter().enumerate() {
            let mut bad = false;
            for &c in &texel[..2] {
                if !c.is_finite() {
                    bad = true;
                } else {
                    min = min.min(c);
                    max = max.max(c);
                }
            }
            if bad {
                non_finite += 1;
                if bad_pixels.len() < 8 {
                    bad_pixels.push(json!({
                        "xy": [index as u32 % 128, index as u32 / 128],
                        "bits": [texel[0].to_bits() >> 16, texel[1].to_bits() >> 16],
                    }));
                }
            }
        }
        return Ok(json!({
            "lut": true,
            "min": if min.is_finite() { Some(min) } else { None },
            "max": if max.is_finite() { Some(max) } else { None },
            "non_finite": non_finite,
            "bad_pixels": bad_pixels,
        }));
    }
    let faces = env
        .readback_cube(ctx, mip)
        .map_err(internal_err("环境 cubemap 读回失败"))?;
    // 每个面是 RGBA 四通道行主序：边长 = sqrt(浮点数 / 4)
    let size = faces.first().map_or(0, |rgba| (rgba.len() / 4).isqrt());
    let mut rows = Vec::with_capacity(faces.len());
    for (face, rgba) in faces.iter().enumerate() {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut non_finite = 0u64;
        for texel in rgba.as_chunks::<4>().0 {
            for &c in &texel[..3] {
                if !c.is_finite() {
                    non_finite += 1;
                    continue;
                }
                min = min.min(c);
                max = max.max(c);
                sum += f64::from(c);
            }
        }
        let center = if size > 0 {
            let offset = ((size / 2) * size + size / 2) * 4;
            rgba.get(offset..offset + 3).map(<[f32]>::to_vec)
        } else {
            None
        };
        let count = (rgba.len() / 4 * 3).max(1);
        rows.push(json!({
            "face": face,
            "min": if min.is_finite() { Some(min) } else { None },
            "max": if max.is_finite() { Some(max) } else { None },
            "mean": sum / count as f64,
            "non_finite": non_finite,
            "center_rgb": center,
        }));
    }
    Ok(json!({ "mip": mip, "size": size, "faces": rows }))
}

/// 设置现值（`ibl_set` 回显与 `ibl_stats` 共用）。
fn current(ibl: &IblSettings) -> Value {
    json!({
        "enabled": ibl.enabled,
        "intensity": ibl.intensity,
        "mode": ibl.mode,
        "diffuse_mode": mode_name(ibl.mode),
    })
}

/// 漫反射编码的字符串名（与 `ibl_set` 的入参同一名称表）。
fn mode_name(mode: u32) -> &'static str {
    if mode == IBL_MODE_PROBE16 {
        "probe16"
    } else {
        "sh_l2"
    }
}
