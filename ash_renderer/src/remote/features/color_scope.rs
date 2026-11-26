//! 颜色校验方法族（9.9）：`color_scope_set`（总开关 / 单帧采样）与
//! `color_scope_get`（最近一次 bins 快照）。
//!
//! 与 egui「颜色」页同源：两边读同一份 bins、同一套常量（轴 −16…+8 EV、256 bin、
//! CbCr 64×64），CPU 只解析不另算统计口径。`color_scope_get` 缺省读最近一次采样
//! （带 `frame_age` / `stale`），`force: true` 让下一帧重采一次（等价面板的
//! 「单帧采样」按钮），调用方随后再取即可。
//!
//! 单位：A/B 域轴的 bin 中心是 log2 EV（−16…+8，0.09375 EV/bin），C 域是 sRGB
//! 编码后的 0…1（1/256 per bin）；`CbCr` 的行是 Cr、列是 Cb（各 64 格）。

use bevy::{prelude::*, remote::BrpResult};
use serde::Deserialize;
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    ColorScopeDomain, ColorScopeSettings, ColorScopeSnapshot, ColorScopeTargets, Context,
    COLOR_SCOPE_AXIS_MIN_EV, COLOR_SCOPE_AXIS_SPAN_EV, COLOR_SCOPE_BINS_U32, COLOR_SCOPE_CBCR_EDGE,
    COLOR_SCOPE_STRIDE,
};

use crate::remote::common::params::{
    internal_err, invalid_params, parse_params_or_default, res, res_mut,
};

/// `color_scope_set` 的可省字段（全缺省 = 不改）。
#[derive(Deserialize, Default)]
struct SetParams {
    enabled: Option<bool>,
    once: Option<bool>,
}

/// `color_scope_get` 的可省字段。
#[derive(Deserialize, Default)]
struct GetParams {
    /// 帧槽；缺省用最近一次录制的槽。
    slot: Option<u32>,
    /// 为真则请求下一帧重采一次（不打开总开关），本次返回仍可能是旧样本。
    force: Option<bool>,
    /// 是否带 CbCr 64×64 全量（默认带；脚本只做直方图核对时可关掉省体积）。
    cbcr: Option<bool>,
}

/// `ash_renderer/color_scope_set`：颜色校验总开关与单帧采样。
/// 请求 `{"enabled": bool?, "once": bool?}`，至少一项；`once` 只录一帧后自动复位。
///
/// # Errors
/// 参数为空、字段类型不对，或设置资源缺席。
#[system]
pub(crate) fn color_scope_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetParams = parse_params_or_default(params)?;
    if p.enabled.is_none() && p.once.is_none() {
        return Err(invalid_params(
            "params 缺失：需要 enabled 或 once 之一（可同时给）",
        ));
    }
    let mut settings = res_mut::<ColorScopeSettings>(world)?;
    if let Some(enabled) = p.enabled {
        settings.enabled = enabled;
    }
    if let Some(once) = p.once {
        settings.once = once;
    }
    Ok(json!({ "enabled": settings.enabled, "once": settings.once }))
}

/// `ash_renderer/color_scope_get`：读最近一次 bins 快照（面板同源解析）。
///
/// 请求 `{"slot": u32?, "force": bool?, "cbcr": bool?}`；返回头（magic/尺寸/
/// stride/帧号/曝光快照）、三域 luma + RGB parade + 计数 + 均值，以及 `frame_age`
/// / `stale` 与开关状态。
///
/// # Errors
/// 资源缺席（Vulkan 未 init / 禁渲染宿主）或 bins 读回失败。
#[system]
pub(crate) fn color_scope_get(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GetParams = parse_params_or_default(params)?;
    // 先登记"下一帧重采"（若请求），再读——本次拿到的可能还是上一帧样本。
    let requested = p.force.unwrap_or(false);
    if requested {
        res_mut::<ColorScopeSettings>(world)?.once = true;
    }
    let (snapshot, enabled, once, frame_age, slot) = {
        let ctx = res::<Context>(world)?;
        let targets = res::<ColorScopeTargets>(world)?;
        let settings = res::<ColorScopeSettings>(world)?;
        let slot = p.slot.map_or_else(|| targets.last_slot(), |s| s as usize);
        let snapshot = targets
            .read_bins(ctx, slot)
            .map_err(internal_err("颜色校验 bins 读失败"))?;
        let frame_age = targets.frame_index().saturating_sub(snapshot.frame_index());
        (snapshot, settings.enabled, settings.once, frame_age, slot)
    };
    let mut out = snapshot_json(&snapshot, frame_age, p.cbcr.unwrap_or(true));
    out["enabled"] = json!(enabled);
    out["once"] = json!(once);
    out["requested"] = json!(requested);
    out["slot"] = json!(slot);
    Ok(out)
}

/// 快照 → JSON（面板/脚本共用同一字段名）。
fn snapshot_json(snapshot: &ColorScopeSnapshot, frame_age: u64, include_cbcr: bool) -> Value {
    let (width, height) = snapshot.extent();
    let mut out = json!({
        "magic_ok": snapshot.magic_ok(),
        "width": width,
        "height": height,
        "stride": snapshot.header[1],
        "bins_u32": snapshot.bins.len(),
        "expected_bins_u32": COLOR_SCOPE_BINS_U32,
        "expected_stride": COLOR_SCOPE_STRIDE,
        "frame_index": snapshot.frame_index(),
        "frame_age": frame_age,
        "stale": frame_age > 4,
        "exposure_state": snapshot.exposure_state(),
        "axis": {
            "a_b": { "kind": "log2_ev", "min": COLOR_SCOPE_AXIS_MIN_EV,
                     "span": COLOR_SCOPE_AXIS_SPAN_EV, "bins": 256 },
            "c": { "kind": "srgb_unit", "min": 0.0, "span": 1.0, "bins": 256 },
            "cbcr": { "kind": "bt709_cbcr", "edge": COLOR_SCOPE_CBCR_EDGE },
        },
        "domains": {
            "a": domain_json(snapshot, ColorScopeDomain::A),
            "b": domain_json(snapshot, ColorScopeDomain::B),
            "c": domain_json(snapshot, ColorScopeDomain::C),
        },
    });
    if include_cbcr {
        out["cbcr"] = json!(snapshot.cbcr());
    }
    out
}

/// 单域 JSON：luma 与 RGB parade 直方图、计数、越轴比例、轴均值。
fn domain_json(snapshot: &ColorScopeSnapshot, domain: ColorScopeDomain) -> Value {
    let counts = snapshot.counts(domain);
    let ratios = snapshot.ratios(domain);
    json!({
        "axis": match domain {
            ColorScopeDomain::A | ColorScopeDomain::B => "log2_ev",
            ColorScopeDomain::C => "srgb_unit",
        },
        "luma": snapshot.luma(domain),
        "rgb": [
            snapshot.rgb(domain, 0),
            snapshot.rgb(domain, 1),
            snapshot.rgb(domain, 2),
        ],
        "counts": {
            "samples": counts.samples,
            "below": counts.below,
            "above": counts.above,
            "zero": counts.zero,
            "clip": counts.clip,
        },
        "ratios": {
            "below": ratios.map(|r| r.below),
            "above": ratios.map(|r| r.above),
            "zero": ratios.map(|r| r.zero),
            "clip": ratios.map(|r| r.clip),
        },
        "mean_axis": mean_axis(snapshot, domain),
    })
}

/// luma 直方图按 bin 中心加权的均值（A/B 单位 EV，C 单位 sRGB 0…1）。
fn mean_axis(snapshot: &ColorScopeSnapshot, domain: ColorScopeDomain) -> Option<f32> {
    let counts = snapshot.counts(domain);
    if counts.samples == 0 {
        return None;
    }
    let weighted: f64 = snapshot
        .luma(domain)
        .iter()
        .enumerate()
        .filter(|&(_, &n)| n > 0)
        .map(|(i, &n)| f64::from(n) * f64::from(bin_center(domain, i)))
        .sum();
    Some((weighted / f64::from(counts.samples)) as f32)
}

/// 第 `bin` 格的中心值（与面板刻度的口径一致）。
fn bin_center(domain: ColorScopeDomain, bin: usize) -> f32 {
    match domain {
        ColorScopeDomain::A | ColorScopeDomain::B => ColorScopeSnapshot::axis_bin_ev(bin),
        ColorScopeDomain::C => (bin as f32 + 0.5) / 256.0,
    }
}
