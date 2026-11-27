//! 屏幕空间环境光遮蔽（10.2）：`ssao_set`（开关 + 半径/偏置/强度/步数）与
//! `ssao_stats`（GPU 计时 + 全分辨率 AO 均值/暗部占比 + 参数与新鲜度）。
//!
//! 与 egui「环境光遮蔽」页同源：两边读写同一份 [`SsaoSettings`]。关档时
//! `ssao_stats` 不做图像回读（`mean_ao` / `dark_ratio` 回 `null`）——读数方法
//! 不替设置方打开 GPU 段，关档的逐字节恒等判定才有意义。
//!
//! 回读口径：全分辨率 AO 图（`R32_SFLOAT`）按 [`READBACK_STRIDE`] 抽行采样，
//! 均值与 `ao < 0.8` 占比只用于施工判定与面板显示，不参与绘制决策。

use bevy::{prelude::*, remote::BrpResult};
use serde::Deserialize;
use serde_json::{json, Value};

use ash_macros::system;

use crate::remote::common::params::{
    internal_err, invalid_params, parse_params, parse_params_or_default, res, res_mut,
};
use crate::render::{
    readback_image_region, Context, PerfLedger, SsaoSettings, SsaoTargets,
};

/// 抽样步长（纹素）：每行取 `1 / READBACK_STRIDE` 行、行内每 4 个取 1 个。
///
/// 1600×900 全图 5.8 MB、逐纹素解析要几十毫秒；施工判定只看均值与占比，
/// 1/16 抽样（约 9 万样本）足够稳定，单次调用降到毫秒级。
const READBACK_STRIDE: u32 = 4;

/// 「暗部」阈值：`ao` 低于它算接触阴影覆盖像素（判定线 3 的口径）。
const DARK_AO: f32 = 0.8;

/// `ssao_set` 的可省字段（全缺省 = 不改任何字段）。
#[derive(Deserialize, Default)]
struct SetParams {
    enabled: Option<bool>,
    radius_m: Option<f32>,
    bias: Option<f32>,
    intensity: Option<f32>,
    steps: Option<u32>,
    /// 全分辨率诊断模式（判定线 4 对照用），缺省不改。
    full_res: Option<bool>,
    freeze_rotation: Option<bool>,
}

/// `ssao_stats` 的可省字段。
#[derive(Deserialize, Default)]
struct StatsParams {
    /// 帧槽；缺省用最近一次写完的槽（没写过则 0）。
    slot: Option<u32>,
    /// 只统计一个屏幕矩形 `[x, y, w, h]`（像素，左下原点外的图像行）。
    ///
    /// 判定线 2/3 要在「只看平地的窗口」和「只看柱础的窗口」上取读数：整帧均值里
    /// 天空与墙面都是 1.0，会把接触区的信号稀释；给定矩形后 `mean_ao` / `min_ao` /
    /// `max_ao` / `dark_ratio` 都只覆盖该矩形。缺省 = 整帧。越界部分被裁到画面内，
    /// 裁剪后为空则回 `null` 读数（不回退成整帧，避免读数口径悄悄变了）。
    rect: Option<[u32; 4]>,
}

/// `ash_renderer/ssao_set`：SSAO 开关与采样参数。
/// 请求 `{"enabled": bool?, "radius_m": f32?, "bias": f32?, "intensity": f32?, "steps": u32?}`，
/// 至少给一项；缺省字段保持当前值，回写生效后的全部字段。
///
/// # Errors
/// 参数为空、字段越界，或设置资源缺席。
#[system]
pub(crate) fn ssao_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetParams = parse_params_or_default(params)?;
    if p.enabled.is_none()
        && p.radius_m.is_none()
        && p.bias.is_none()
        && p.intensity.is_none()
        && p.steps.is_none()
        && p.full_res.is_none()
        && p.freeze_rotation.is_none()
    {
        return Err(invalid_params(
            "params 缺失：需要 enabled / radius_m / bias / intensity / steps / full_res / freeze_rotation 之一",
        ));
    }
    let mut settings = res_mut::<SsaoSettings>(world)?;
    // 先全部校验再落值：越界请求整体拒绝，不留半套参数。
    if let Some(v) = p.radius_m
        && !(v.is_finite() && v > 0.0 && v <= 10.0)
    {
        return Err(invalid_params("radius_m 需在 (0, 10] 米内"));
    }
    if let Some(v) = p.bias
        && !(v.is_finite() && (0.0..=1.0).contains(&v))
    {
        return Err(invalid_params("bias 需在 [0, 1] 米内"));
    }
    if let Some(v) = p.intensity
        && !(v.is_finite() && (0.0..=4.0).contains(&v))
    {
        return Err(invalid_params("intensity 需在 [0, 4] 内"));
    }
    if let Some(v) = p.steps
        && !(1..=16).contains(&v)
    {
        return Err(invalid_params("steps 需在 [1, 16] 内"));
    }
    if let Some(v) = p.enabled {
        settings.enabled = v;
    }
    if let Some(v) = p.radius_m {
        settings.radius_m = v;
    }
    if let Some(v) = p.bias {
        settings.bias = v;
    }
    if let Some(v) = p.intensity {
        settings.intensity = v;
    }
    if let Some(v) = p.steps {
        settings.steps = v;
    }
    if let Some(v) = p.full_res {
        settings.full_res = v;
    }
    if let Some(v) = p.freeze_rotation {
        settings.freeze_rotation = v;
    }
    Ok(json!({
        "enabled": settings.enabled,
        "radius_m": settings.radius_m,
        "bias": settings.bias,
        "intensity": settings.intensity,
        "steps": settings.steps,
        "full_res": settings.full_res,
        "freeze_rotation": settings.freeze_rotation,
    }))
}

/// `ash_renderer/ssao_stats`：SSAO 参数、`ssao_ms` 与 AO 图读数。
///
/// 请求 `{"slot": u32?}`；返回开关/参数、`ssao_ms`（PerfLedger p50）、
/// `mean_ao`、`dark_ratio`（`ao < 0.8` 占比）、`extent`、`slot`、`written`。
/// 关档或目标未建时 `mean_ao` / `dark_ratio` 为 `null`，不触发回读。
///
/// # Errors
/// 资源缺席（Vulkan 未 init / 禁渲染宿主）或 AO 图读回失败。
#[system]
pub(crate) fn ssao_stats(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: StatsParams = parse_params_or_default(params)?;
    let settings = res::<SsaoSettings>(world)?;
    let targets = res::<SsaoTargets>(world)?;
    let extent = targets.full_extent();
    let written = targets.written_slot();
    let slot = p.slot.map_or_else(|| written.unwrap_or(0), |s| s as usize);
    let (enabled, radius_m, bias, intensity, steps, full_res, freeze_rotation) = (
        settings.enabled,
        settings.radius_m,
        settings.bias,
        settings.intensity,
        settings.steps,
        settings.full_res,
        settings.freeze_rotation,
    );
    let ssao_ms = res::<PerfLedger>(world)?
        .percentile(0.5, |s| s.gpu_ssao_ms)
        .map(f64::from);
    let (rx, ry, rw, rh) = stats_region(p.rect, extent);
    // 关档 / 目标没建 / 该槽还没写过：不做回读（判定线 1b：关档 stats 不碰 GPU）。
    if !enabled || extent.width == 0 || extent.height == 0 {
        return Ok(json!({
            "enabled": enabled,
            "radius_m": radius_m,
            "bias": bias,
            "intensity": intensity,
            "steps": steps,
            "full_res": full_res,
            "freeze_rotation": freeze_rotation,
            "ssao_ms": ssao_ms,
            "mean_ao": Value::Null,
            "min_ao": Value::Null,
            "max_ao": Value::Null,
            "dark_ratio": Value::Null,
            "dark_ao_threshold": DARK_AO,
            "extent": { "width": extent.width, "height": extent.height },
            "region": { "x": rx, "y": ry, "width": rw, "height": rh },
            "slot": slot,
            "written": written,
        }));
    }
    let bytes = if rw == 0 || rh == 0 {
        // 矩形被裁空：不回读，读数回 null（口径是「给定窗口内」而不是退成整帧）。
        Vec::new()
    } else {
        let ctx = res::<Context>(world)?;
        let image = targets.full_image(slot);
        readback_image_region(
            ctx,
            image,
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
            4,
        )
        .map_err(internal_err("SSAO 全分辨率 AO 回读失败"))?
    };
    let (sum, dark, count, min_ao, max_ao) = ao_histogram(&bytes, rw);
    let mean_ao = if count == 0 { None } else { Some(sum / count as f64) };
    let dark_ratio = if count == 0 {
        None
    } else {
        Some(dark as f64 / count as f64)
    };
    let (min_ao, max_ao) = if count == 0 {
        (None, None)
    } else {
        (Some(min_ao), Some(max_ao))
    };
    Ok(json!({
        "enabled": enabled,
        "radius_m": radius_m,
        "bias": bias,
        "intensity": intensity,
        "steps": steps,
        "full_res": full_res,
        "freeze_rotation": freeze_rotation,
        "ssao_ms": ssao_ms,
        "mean_ao": mean_ao,
        "min_ao": min_ao,
        "max_ao": max_ao,
        "dark_ratio": dark_ratio,
        "dark_ao_threshold": DARK_AO,
        "extent": { "width": extent.width, "height": extent.height },
        "region": { "x": rx, "y": ry, "width": rw, "height": rh },
        "slot": slot,
        "written": written,
        "sample_stride": READBACK_STRIDE,
    }))
}

/// `ssao_dump` 的参数：写盘路径 + 可选帧槽。
#[derive(Deserialize)]
struct DumpParams {
    path: String,
    slot: Option<u32>,
}

/// `ash_renderer/ssao_dump`：把全分辨率 AO 图按行原样写成小端 `f32` 原始文件。
///
/// 判定线 2/3/4 要在 Python 里算全像素均值、最小值与半/全分辨率 p99，
/// `ssao_stats` 的 1/16 抽样只够面板显示。文件没有头：宽高取返回的 `extent`，
/// 元素是 4 字节小端 f32；`path` 建议落在仓库根 `.temp/`（证据不入库）。
///
/// # Errors
/// 参数缺失、AO 目标未建立，或回读/写盘失败。
#[system]
pub(crate) fn ssao_dump(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: DumpParams = parse_params(params, "params 缺失：需要 path")?;
    let targets = res::<SsaoTargets>(world)?;
    let extent = targets.full_extent();
    if extent.width == 0 || extent.height == 0 {
        return Err(internal_err("SSAO 目标尚未建立")("extent 为 0"));
    }
    let written = targets.written_slot();
    let slot = p.slot.map_or_else(|| written.unwrap_or(0), |s| s as usize);
    let ctx = res::<Context>(world)?;
    let bytes = readback_image_region(
        ctx,
        targets.full_image(slot),
        ash::vk::ImageAspectFlags::COLOR,
        ash::vk::ImageLayout::GENERAL,
        ash::vk::Offset2D { x: 0, y: 0 },
        extent,
        4,
    )
    .map_err(internal_err("SSAO 全分辨率 AO 回读失败"))?;
    std::fs::write(&p.path, &bytes).map_err(internal_err("写 AO 原始文件失败"))?;
    Ok(json!({
        "path": p.path,
        "bytes": bytes.len(),
        "extent": { "width": extent.width, "height": extent.height },
        "slot": slot,
        "written": written,
    }))
}

/// 解析并裁剪 `ssao_stats` 的统计矩形：缺省整帧，越界部分裁到画面内。
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

/// 按 [`READBACK_STRIDE`] 抽行/抽纹素统计 AO：返回（和、暗部数、样本数、最小、最大）。
///
/// `bytes` 是 `width × height` 的 `R32_SFLOAT` 连续回读；抽行保证每行都能覆盖到，
/// 避免只统计上半张画面。`min_ao` / `max_ao` 供判定线 2（「最大暗化为 0」）与
/// 判定线 3（接触区下界）直接读；`width == 0`（矩形裁空）返回空统计。
fn ao_histogram(bytes: &[u8], width: u32) -> (f64, u64, u64, f32, f32) {
    let stride = READBACK_STRIDE as usize;
    let width = width as usize;
    let mut sum = 0.0f64;
    let mut dark = 0u64;
    let mut count = 0u64;
    let mut min_ao = f32::INFINITY;
    let mut max_ao = f32::NEG_INFINITY;
    if width == 0 {
        return (0.0, 0, 0, 0.0, 0.0);
    }
    for (index, texel) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let x = index % width;
        let y = index / width;
        if !x.is_multiple_of(stride) || !y.is_multiple_of(stride) {
            continue;
        }
        let ao = f32::from_le_bytes([texel[0], texel[1], texel[2], texel[3]]);
        if !ao.is_finite() {
            continue;
        }
        sum += f64::from(ao);
        min_ao = min_ao.min(ao);
        max_ao = max_ao.max(ao);
        if ao < DARK_AO {
            dark += 1;
        }
        count += 1;
    }
    (sum, dark, count, min_ao, max_ao)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk;

    #[test]
    fn histogram_samples_every_fourth_texel_and_row() {
        // 4×4 图：全 1.0，除 (0,0) 与 (2,2) 为 0.5；抽样步长 4 → 只取 (0,0)。
        let mut bytes = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let v = if (x == 0 && y == 0) || (x == 2 && y == 2) {
                    0.5f32
                } else {
                    1.0f32
                };
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        let (sum, dark, count, min_ao, max_ao) = ao_histogram(&bytes, 4);
        assert_eq!(count, 1);
        assert_eq!(dark, 1);
        assert!((sum - 0.5).abs() < 1e-6);
        assert!((min_ao - 0.5).abs() < 1e-6);
        assert!((max_ao - 0.5).abs() < 1e-6);
    }

    #[test]
    fn histogram_skips_non_finite_values() {
        // 1×8 列：抽样只取 y = 0 与 y = 4；(0,0) 放 NaN 应被跳过，只统计 (0,4)。
        let mut bytes = Vec::new();
        for y in 0..8 {
            let v = if y == 0 {
                f32::NAN
            } else if y == 4 {
                0.25f32
            } else {
                1.0f32
            };
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let (sum, dark, count, min_ao, max_ao) = ao_histogram(&bytes, 1);
        assert_eq!(count, 1);
        assert_eq!(dark, 1);
        assert!((sum - 0.25).abs() < 1e-6);
        assert!((min_ao - 0.25).abs() < 1e-6);
        assert!((max_ao - 0.25).abs() < 1e-6);
    }

    #[test]
    fn empty_width_returns_empty_histogram() {
        // 矩形裁空（width == 0）不能除零，读数应回 count 0。
        let (sum, dark, count, min_ao, max_ao) = ao_histogram(&[], 0);
        assert_eq!(count, 0);
        assert_eq!(dark, 0);
        assert!(sum.abs() < 1e-12);
        assert!(min_ao.abs() < 1e-12);
        assert!(max_ao.abs() < 1e-12);
    }

    #[test]
    fn stats_region_clamps_to_extent() {
        let extent = vk::Extent2D {
            width: 100,
            height: 50,
        };
        assert_eq!(stats_region(None, extent), (0, 0, 100, 50));
        assert_eq!(stats_region(Some([10, 5, 20, 10]), extent), (10, 5, 20, 10));
        // 起点越界 + 尺寸越界：裁到画面内。
        assert_eq!(stats_region(Some([90, 45, 20, 10]), extent), (90, 45, 10, 5));
        // 起点完全在画面外：裁空。
        assert_eq!(stats_region(Some([200, 100, 10, 10]), extent), (100, 50, 0, 0));
    }
}
