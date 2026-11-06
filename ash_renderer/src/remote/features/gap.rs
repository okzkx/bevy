//! GPU 缺口请求读数与模式方法族：`gap_stats` 聚合读数（缺口槽数/去重身份数/
//! 触发行数/陈旧丢弃），`gap_census` 逐槽位图清单（GPU 缺口 vs CPU 参照对账的
//! 原始证据），`gap_set` 写三态模式。
//!
//! 边界：只读快照/只写设置，读数不回灌绘制决策。
//!
//! 读数时点：GapSnapshot 由帧循环在本帧槽 fence 等待后采样（两帧前 compute 的
//! 输出），handler 读的是这份快照，不直读环，避免与在飞 compute 的写竞争。

use std::collections::HashSet;

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::{GapMode, GapSnapshot, InstanceLedger, ResidencySettings, ResidencyStats};
use crate::render::MAX_LOD_LEVELS;

use crate::remote::common::params::{invalid_params, parse_params, res, res_mut};

/// `ash_renderer/gap_stats`：缺口请求聚合读数（只读）——模式快照 + 快照槽
/// 数 + 本 tick 触发行数/累计/陈旧丢弃。
#[system]
pub(crate) fn gap_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let stats = res::<ResidencyStats>(world)?;
    let snapshot = res::<GapSnapshot>(world)?;
    let nonzero = snapshot.bits.iter().filter(|b| **b != 0).count();
    Ok(json!({
        "mode": stats.gap_mode,
        "snapshot_bound": snapshot.bound,
        "snapshot_slots_nonzero": nonzero,
        "gap_slots_last": stats.gap_slots_last,
        "gap_assets_last": stats.gap_assets_last,
        "gap_rows_last": stats.gap_rows_last,
        "gap_rows_total": stats.gap_rows_total,
        "gap_stale_total": stats.gap_stale_total,
    }))
}

/// `ash_renderer/gap_census`：逐槽缺口位图清单（只读）——非零槽列表
/// `[{slot, bits, levels:[...], row}]`，levels = 置位档位号（GPU 缺口 vs CPU
/// 参照对账的原始证据），row = 该槽当前是否对应账本行（false = 陈旧缺口，
/// 合并时按 gap_stale 丢弃）。
#[system]
pub(crate) fn gap_census(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let row_slots: HashSet<u32> = res::<InstanceLedger>(world)?
        .rows()
        .filter_map(|(_, row)| row.instance_slot)
        .collect();
    let snapshot = res::<GapSnapshot>(world)?;
    let slots: Vec<Value> = snapshot
        .bits
        .iter()
        .enumerate()
        .filter(|(_, bits)| **bits != 0)
        .map(|(slot, bits)| {
            let levels: Vec<u32> = (0..MAX_LOD_LEVELS)
                .filter(|level| bits >> level & 1 == 1)
                .collect();
            let row = row_slots.contains(&(slot as u32));
            json!({ "slot": slot, "bits": bits, "levels": levels, "row": row })
        })
        .collect();
    Ok(json!({
        "bound": snapshot.bound,
        "slots": slots,
        "nonzero": slots.len(),
    }))
}

/// `ash_renderer/gap_set` 参数：mode 字符串（off/gpu_only/merged，缺省 = 不动）。
#[derive(serde::Deserialize)]
struct GapSetParams {
    #[serde(default)]
    mode: Option<String>,
}

/// `ash_renderer/gap_set`：写缺口请求模式（三态 A/B 驱动）。返回写入后的状态。
///
/// # Errors
/// 参数缺失/未知形态名。
#[system]
pub(crate) fn gap_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: GapSetParams = parse_params(params, "params 缺失：需要对象（mode 可选）")?;
    let mut settings = res_mut::<ResidencySettings>(world)?;
    if let Some(mode) = &p.mode {
        settings.gap_mode = GapMode::parse(mode).map_err(invalid_params)?;
    }
    Ok(json!({ "mode": settings.gap_mode.as_str() }))
}
