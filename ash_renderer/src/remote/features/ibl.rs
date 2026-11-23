//! 环境光照方法族：`ibl_set`（开关/强度，缺省字段不动）与 `ibl_stats`（设置、SH-L2
//! 系数、两条自检误差、捕获账本与读数的只读回显）。
//!
//! 读数允许滞后（BRP 不是正确性路径）：SH 与 stats 缓冲在捕获帧由 compute 写，本方法
//! 用一次性拷贝 + fence 读回，取到的是"某个完整状态"（同 `hiz_census` 口径）。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    readback_buffer, Context, IblEnvironment, IblSettings, PerfLedger, IBL_CUBE_MIPS,
    IBL_SH_BUFFER_SIZE, IBL_STATS_BUFFER_SIZE, IBL_STATS_FLOATS,
};

use crate::remote::common::params::{internal_err, invalid_params, parse_params, res, res_mut};

/// `ash_renderer/ibl_set`：环境光照开关与强度。请求 `{"enabled": bool, "intensity": f32?}`，
/// 缺省字段不动；回显当前值。
#[system]
pub(crate) fn ibl_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: serde_json::Map<String, Value> =
        parse_params(params, "params 缺失：需要 enabled/intensity 之一")?;
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
    if echo.as_object().is_none_or(|o| o.is_empty()) {
        return Err(invalid_params("无有效字段（enabled/intensity 任一）"));
    }
    Ok(json!({ "applied": echo, "current": current(&ibl) }))
}

/// `ash_renderer/ibl_stats`：设置、SH-L2 系数、两条自检误差与捕获账本。Vulkan 资源
/// 缺席（禁渲染宿主）时只回设置。
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

/// `ash_renderer/ibl_census` 参数：`mip` 可选（缺省 0 = 锐利源；1..8 = 预滤波级）。
#[derive(serde::Deserialize)]
struct IblCensusParams {
    #[serde(default)]
    mip: Option<u32>,
    /// true = 读回 DFG LUT 而不是 cubemap。
    #[serde(default)]
    lut: bool,
}

/// `ash_renderer/ibl_census`：读回环境 cubemap 某一级的六个面，报 min/max/mean、
/// 非有限值计数与中心像素（面方向与接缝的取证口）。只进验证，不进绘制路径。
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
    if p.lut {
        let rgba = env
            .readback_dfg(ctx)
            .map_err(internal_err("DFG LUT 读回失败"))?;
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
    })
}
