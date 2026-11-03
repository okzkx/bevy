//! 缺口快照的帧侧单元：缺口请求链（6.4 细粒度流送）里"GPU 侧发现缺什么 → CPU 侧
//! 驻留管理器补传"的桥——帧起采样缺口标记环成快照，驻留管理器 tick 与 BRP census
//! 都只读快照，不直读环。

use bevy::prelude::*;

use crate::{
    common::syntax::ResultTierExt,
    render::{InstanceTables, GAP_MARK_SIZE},
    scene::GapSnapshot,
};

use super::ring::read_ring;
use super::PassFrame;

/// 缺口快照采样：帧起读缺口标记环（本帧槽 = 两帧前 compute 输出）。坑：驻留管理器
/// tick 在帧提交之后跑，直读环会与在飞 compute 写竞争，所以 tick 与 BRP census 都读
/// 本快照。
pub(super) fn sample_gaps(
    pf: &PassFrame,
    gap: &mut GapSnapshot,
    instances: &mut InstanceTables,
    cull_skipped: bool,
) -> Result<(), BevyError> {
    if pf.bound > 0 {
        let len = pf.bound as usize * GAP_MARK_SIZE as usize;
        gap.bits = read_ring(instances.gap_mark_ring(pf.slot), len)
            .or_fatal("缺口标记环读数失败，渲染链无法继续")?;
        // 缺席槽清零由 cull dispatch 负责（覆盖全部 bound）；本帧未 dispatch（剔除与
        // 阴影都关）时缺席槽会残留陈旧位图，由 host 补零（在场槽由本帧 build_indirect
        // 照常写新值）。坑：补零必须在快照读数之后，先清会抹掉两帧前 build_indirect
        // 写下的缺口，驻留管理器永远看不到
        if cull_skipped {
            instances
                .gap_mark_ring(pf.slot)
                .write(0, &vec![0u8; len])
                .or_fatal("缺口标记环零填充失败，渲染链无法继续")?;
        }
    } else {
        gap.bits.clear();
    }
    gap.bound = pf.bound;
    Ok(())
}
