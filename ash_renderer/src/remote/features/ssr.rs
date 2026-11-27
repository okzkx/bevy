//! 屏幕空间反射（10.3）：`ssr_set`（开关 + 步数/距离/厚度/门控/边缘/强度）与
//! `ssr_stats`（GPU 计时 + 反射图命中率/权重均值 + 参数与新鲜度）。
//!
//! 与 egui「环境光照」页的「屏幕空间反射」小节同源：两边读写同一份 [`ReflectionSettings`]。
//! 关档时 `ssr_stats` 不做图像回读（`hit_ratio` / `mean_w` 回 `null`）——读数方法不替设置方
//! 打开 GPU 段，关档的逐字节恒等判定才有意义。
//!
//! 回读口径：半分辨率反射图（`R16G16B16A16_SFLOAT`，rgb = 命中色、a = 权重 w）按
//! [`READBACK_STRIDE`] 抽行采样。`hit_ratio` 是 `w > 0` 的纹素占比（判定线 3 要它恰为 0），
//! `mean_w` 是命中像素材值之外的全体均值。只用于施工判定与面板显示，不参与绘制决策。

use bevy::{prelude::*, remote::BrpResult};
use serde::Deserialize;
use serde_json::{json, Value};

use ash_macros::system;

use crate::remote::common::params::{
    internal_err, invalid_params, parse_params_or_default, res, res_mut,
};
use crate::render::{readback_image_region, Context, PerfLedger, ReflectionSettings, ReflectionTargets};

/// 抽样步长（纹素）：每行取 `1 / READBACK_STRIDE` 行、行内每 4 个取 1 个。
///
/// 半分辨率图 800×450、每纹素 8 字节；1/16 抽样（约 2.2 万样本）足够判定命中率与均值，
/// 单次调用降到毫秒级。
const READBACK_STRIDE: u32 = 4;

/// 反射图的每纹素字节数（`R16G16B16A16_SFLOAT`）。
const REFLECTION_BYTES: u64 = 8;

/// 判「命中」的权重阈值：w 由 `gate_roughness · gate_distance · edge_fade · intensity`
/// 连乘得到，未命中时着色器写 0；这里用 1e-4 抵抗 16F 的舍入。
const HIT_W: f32 = 1e-4;

/// 反射图每纹素 8 字节（4 × f16）。
const REFLECTION_TEXEL_BYTES: usize = 8;

/// `ssr_set` 的可省字段（全缺省 = 不改任何字段）。
#[derive(Deserialize, Default)]
struct SetParams {
    enabled: Option<bool>,
    steps: Option<u32>,
    max_distance_m: Option<f32>,
    thickness_m: Option<f32>,
    roughness_cutoff: Option<f32>,
    edge_fade: Option<f32>,
    intensity: Option<f32>,
}

/// `ssr_stats` 的可省字段。
#[derive(Deserialize, Default)]
struct StatsParams {
    /// 帧槽；缺省用最近一次写完的槽（没写过则 0）。
    slot: Option<u32>,
    /// 只统计一个半分辨率图上的矩形 `[x, y, w, h]`（像素）。判定线 2 要在镜面地砖的窗口上
    /// 看反射，整帧均值会把天空与墙面稀释掉。缺省 = 整张图。越界部分裁到图内，裁空回 `null`。
    rect: Option<[u32; 4]>,
}

/// `ash_renderer/ssr_set`：SSR 开关与步进参数。
/// 请求 `{"enabled": bool?, "steps": u32?, "max_distance_m": f32?, "thickness_m": f32?, "roughness_cutoff": f32?, "edge_fade": f32?, "intensity": f32?}`，
/// 至少给一项；缺省字段保持当前值，回写生效后的全部字段。
///
/// # Errors
/// 参数为空、字段越界，或设置资源缺席。
#[system]
pub(crate) fn ssr_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetParams = parse_params_or_default(params)?;
    if p.enabled.is_none()
        && p.steps.is_none()
        && p.max_distance_m.is_none()
        && p.thickness_m.is_none()
        && p.roughness_cutoff.is_none()
        && p.edge_fade.is_none()
        && p.intensity.is_none()
    {
        return Err(invalid_params(
            "params 缺失：需要 enabled / steps / max_distance_m / thickness_m / roughness_cutoff / edge_fade / intensity 之一",
        ));
    }
    let mut settings = res_mut::<ReflectionSettings>(world)?;
    // 先全部校验再落值：越界请求整体拒绝，不留半套参数。
    if let Some(v) = p.steps
        && !(1..=256).contains(&v)
    {
        return Err(invalid_params("steps 需在 [1, 256] 内"));
    }
    if let Some(v) = p.max_distance_m
        && !(v.is_finite() && (0.1..=200.0).contains(&v))
    {
        return Err(invalid_params("max_distance_m 需在 [0.1, 200] 米内"));
    }
    if let Some(v) = p.thickness_m
        && !(v.is_finite() && (0.0..=5.0).contains(&v))
    {
        return Err(invalid_params("thickness_m 需在 [0, 5] 米内"));
    }
    for (name, v) in [
        ("roughness_cutoff", p.roughness_cutoff),
        ("edge_fade", p.edge_fade),
    ] {
        if let Some(v) = v
            && !(v.is_finite() && (0.0..=1.0).contains(&v))
        {
            return Err(invalid_params(format!("{name} 需在 [0, 1] 内")));
        }
    }
    if let Some(v) = p.intensity
        && !(v.is_finite() && (0.0..=4.0).contains(&v))
    {
        return Err(invalid_params("intensity 需在 [0, 4] 内"));
    }
    if let Some(v) = p.enabled {
        settings.enabled = v;
    }
    if let Some(v) = p.steps {
        settings.steps = v;
    }
    if let Some(v) = p.max_distance_m {
        settings.max_distance_m = v;
    }
    if let Some(v) = p.thickness_m {
        settings.thickness_m = v;
    }
    if let Some(v) = p.roughness_cutoff {
        settings.roughness_cutoff = v;
    }
    if let Some(v) = p.edge_fade {
        settings.edge_fade = v;
    }
    if let Some(v) = p.intensity {
        settings.intensity = v;
    }
    Ok(json!({
        "enabled": settings.enabled,
        "steps": settings.steps,
        "max_distance_m": settings.max_distance_m,
        "thickness_m": settings.thickness_m,
        "roughness_cutoff": settings.roughness_cutoff,
        "edge_fade": settings.edge_fade,
        "intensity": settings.intensity,
    }))
}

/// `ash_renderer/ssr_stats`：SSR 参数、`ssr_ms` 与反射图读数。
///
/// 请求 `{"slot": u32?, "rect": [x, y, w, h]?}`；返回开关/参数、`ssr_ms`（PerfLedger p50）、
/// `hit_ratio`、`mean_w`、`extent`（半分辨率图尺寸）、`full_extent`、`slot`、`written`。
/// 关档或目标未建时 `hit_ratio` / `mean_w` 为 `null`，不触发回读。
///
/// # Errors
/// 资源缺席（Vulkan 未 init / 禁渲染宿主）或反射图读回失败。
#[system]
pub(crate) fn ssr_stats(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: StatsParams = parse_params_or_default(params)?;
    let settings = res::<ReflectionSettings>(world)?;
    let targets = res::<ReflectionTargets>(world)?;
    let extent = targets.half_extent();
    let full_extent = targets.full_extent();
    let written = targets.written_slot();
    let slot = p.slot.map_or_else(|| written.unwrap_or(0), |s| s as usize);
    let (enabled, steps, max_distance_m, thickness_m, roughness_cutoff, edge_fade, intensity) = (
        settings.enabled,
        settings.steps,
        settings.max_distance_m,
        settings.thickness_m,
        settings.roughness_cutoff,
        settings.edge_fade,
        settings.intensity,
    );
    let ssr_ms = res::<PerfLedger>(world)?
        .percentile(0.5, |s| s.gpu_ssr_ms)
        .map(f64::from);
    let (rx, ry, rw, rh) = stats_region(p.rect, extent);
    if !enabled || extent.width == 0 || extent.height == 0 {
        return Ok(json!({
            "enabled": enabled,
            "steps": steps,
            "max_distance_m": max_distance_m,
            "thickness_m": thickness_m,
            "roughness_cutoff": roughness_cutoff,
            "edge_fade": edge_fade,
            "intensity": intensity,
            "ssr_ms": ssr_ms,
            "hit_ratio": Value::Null,
            "mean_w": Value::Null,
            "extent": { "width": extent.width, "height": extent.height },
            "full_extent": { "width": full_extent.width, "height": full_extent.height },
            "region": { "x": rx, "y": ry, "width": rw, "height": rh },
            "slot": slot,
            "written": written,
        }));
    }
    let bytes = if rw == 0 || rh == 0 {
        Vec::new()
    } else {
        let ctx = res::<Context>(world)?;
        readback_image_region(
            ctx,
            targets.image(slot),
            ash::vk::ImageAspectFlags::COLOR,
            ash::vk::ImageLayout::GENERAL,
            ash::vk::Offset2D {
                x: rx as i32,
                y: ry as i32,
            },
            ash::vk::Extent2D {
                width: rw,
                height: rh,
            },
            REFLECTION_BYTES,
        )
        .map_err(internal_err("SSR 反射图回读失败"))?
    };
    let (hits, count, mean_w) = reflection_histogram(&bytes);
    let hit_ratio = if count == 0 {
        None
    } else {
        Some(hits as f64 / count as f64)
    };
    Ok(json!({
        "enabled": enabled,
        "steps": steps,
        "max_distance_m": max_distance_m,
        "thickness_m": thickness_m,
        "roughness_cutoff": roughness_cutoff,
        "edge_fade": edge_fade,
        "intensity": intensity,
        "ssr_ms": ssr_ms,
        "hit_ratio": hit_ratio,
        "mean_w": mean_w,
        "hit_threshold": HIT_W,
        "extent": { "width": extent.width, "height": extent.height },
        "full_extent": { "width": full_extent.width, "height": full_extent.height },
        "region": { "x": rx, "y": ry, "width": rw, "height": rh },
        "slot": slot,
        "written": written,
        "sample_stride": READBACK_STRIDE,
    }))
}

/// 解析并裁剪 `ssr_stats` 的统计矩形：缺省整帧，越界部分裁到图内。
fn stats_region(rect: Option<[u32; 4]>, extent: ash::vk::Extent2D) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = match rect {
        Some([x, y, w, h]) => (x, y, w, h),
        None => (0, 0, extent.width, extent.height),
    };
    let x = x.min(extent.width);
    let y = y.min(extent.height);
    let w = w.min(extent.width - x);
    let h = h.min(extent.height - y);
    (x, y, w, h)
}

/// 抽行扫描反射图：数命中纹素（`w > HIT_W`）、样本总数与 `w` 均值。
///
/// `bytes` 是 `height × row_stride` 个 `REFLECTION_TEXEL_BYTES` 字节的行主序数据；f16 的
/// 解码走 [`half_to_f32`](crate::render::half_to_f32)，与 IBL 回读同一套。
fn reflection_histogram(bytes: &[u8]) -> (u32, u32, Option<f64>) {
    let mut hits = 0u32;
    let mut count = 0u32;
    let mut sum = 0.0f64;
    for (i, texel) in bytes
        .chunks(REFLECTION_TEXEL_BYTES)
        .step_by(READBACK_STRIDE as usize)
        .enumerate()
    {
        // 行内再抽：保持与 READBACK_STRIDE 一致的 1/4 列采样
        if i % READBACK_STRIDE as usize != 0 {
            continue;
        }
        let alpha = u16::from_le_bytes([texel[6], texel[7]]);
        let w = crate::render::half_to_f32(alpha);
        if w > HIT_W {
            hits += 1;
        }
        sum += f64::from(w);
        count += 1;
    }
    let mean_w = if count == 0 {
        None
    } else {
        Some(sum / f64::from(count))
    };
    (hits, count, mean_w)
}
