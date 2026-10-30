//! DrawList 组装对照账面：账本组装缓存（[`super::assembly_cache`]）的增量
//! 产物与全量参照走访逐帧比对的开关与计量。driver 的帧循环比对并记账，
//! BRP `draw_stats`/`drawlist_verify_set` 读写。

use bevy::prelude::*;

/// 组装对照账面（BRP 开关对照模式）：开启时帧循环逐帧跑全量参照走访与增量
/// 缓存比对——只进计量，**不驱动绘制**。默认关，稳态零成本。
#[derive(Resource, Default)]
pub struct DrawListVerify {
    /// 对照开关（BRP `drawlist_verify_set` 写）。
    pub enabled: bool,
    pub frames_compared_total: u64,
    /// 非零表示增量结果与全量参照不一致。
    pub mismatches_total: u64,
    /// 最近一次不一致的摘要（行数差/首差位/字段名）。
    pub last_mismatch: Option<String>,
}
