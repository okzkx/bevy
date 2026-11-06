//! 透明层的帧侧单元：取用束（TransparentInput）、透明行数清零与本帧透明层
//! 录制输入准备（transparent_draw）。三条 compute 管线与透明前向管线本体在
//! [`crate::render::passes::transparent`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::{
    common::syntax::ResultTierExt,
    render::{TransparentFrame, TransparentPasses, TransparentPipeline, TransparentTargets},
};

/// 透明层：透明命令链资源、三条 compute 管线和透明前向管线。
#[derive(SystemParam)]
pub(super) struct TransparentInput<'w> {
    pub(super) targets: ResMut<'w, TransparentTargets>,
    pub(super) passes: Res<'w, TransparentPasses>,
    pub(super) pipeline: Res<'w, TransparentPipeline>,
}

impl TransparentInput<'_> {
    /// 本帧槽透明行数清零，collect 在 GPU 上从 0 原子累加。
    pub(super) fn clear_slot_count(&mut self, slot: usize) -> Result<(), BevyError> {
        self.targets
            .count(slot)
            .write(0, &0u32.to_le_bytes())
            .or_fatal("透明行计数清零失败，渲染链无法继续")
    }
}

/// 在场行 > 0 才录透明层；在场行数超过透明行容量时报错退出。
pub(super) fn transparent_draw(
    transparent: &TransparentInput,
    slot: usize,
    count: u32,
) -> Result<Option<TransparentFrame>, BevyError> {
    if count == 0 {
        return Ok(None);
    }
    let (targets, passes) = (&transparent.targets, &transparent.passes);
    Ok(Some(TransparentFrame {
        transparent_set: targets.frame_set(slot),
        collect: Some(
            passes
                .collect(count)
                .or_fatal("透明行收集 dispatch 输入组装失败，渲染链无法继续")?,
        ),
        gather: Some(passes.gather(count)),
        sort: passes.sort_plan(),
        pipeline: transparent.pipeline.rec(),
        t_commands: targets.command_buffer(slot),
        t_keys: targets.key_buffer(slot),
        command_count: count,
    }))
}
