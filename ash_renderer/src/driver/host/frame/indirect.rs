//! indirect 命令组装的帧侧单元：取用束（IndirectInput）与录制输入准备
//!（create_indirect_draw）。build_indirect compute 与图形侧一次
//! cmd_draw_indexed_indirect 在 [`crate::render::passes::visibility::indirect`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::{
    common::syntax::{OnceLog, ResultTierExt},
    render::{IndirectDraw, IndirectPass, IndirectStats, INDIRECT_COMMAND_SIZE},
};

/// indirect 命令组装：compute 管线 + 诊断账面（只进计量）。
#[derive(SystemParam)]
pub(super) struct IndirectInput<'w> {
    pub(super) pass: Res<'w, IndirectPass>,
    pub(super) stats: ResMut<'w, IndirectStats>,
}

/// 准备本帧 indirect 命令组装录制输入：在场行数 = GPU 命令条数（来自账本，非可见集
/// 回读）；空场 = None，场景绘制整段跳过。诊断读数只进计量。
pub(super) fn create_indirect_draw(
    indirect: &mut IndirectInput,
    count: u32,
    visible: u32,
    walk_rows: usize,
    pending_rows: usize,
    first: &mut OnceLog,
) -> Result<Option<IndirectDraw>, BevyError> {
    let dispatch = if count > 0 {
        first.log(|| info!(
            "indirect 提交首跑：在场行 {count}，命令环 {}B/条，\
             图形侧一次 cmd_draw_indexed_indirect（图形 push range 归零，firstInstance 携带槽号）",
            INDIRECT_COMMAND_SIZE,
        ));
        Some(
            indirect
                .pass
                .draw(count)
                .or_fatal("indirect dispatch 输入组装失败，渲染链无法继续")?,
        )
    } else {
        None
    };
    let stats = &mut indirect.stats;
    stats.draw_calls_recorded = u32::from(count > 0);
    stats.command_count = count;
    stats.zero_instance_count = count - visible;
    stats.walk_rows = walk_rows;
    stats.pending_rows = pending_rows;
    Ok(dispatch)
}
