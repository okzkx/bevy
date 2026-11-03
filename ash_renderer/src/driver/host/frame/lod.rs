//! GPU LOD 选档的帧侧单元：取用束（LodInput）与录制输入准备（create_lod_draw）。
//! 选档 compute 与迟滞判据在 [`crate::render::passes::visibility::lod`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::{
    common::syntax::{OnceLog, ResultTierExt},
    render::{InstanceTables, LodDiagnostics, LodDraw, LodPass, LodSettings, LOD_LEVEL_SIZE, MAX_LOD_LEVELS},
};

use super::ring::{fill_u32, read_ring};
use super::PassFrame;

/// GPU LOD 选档：compute 管线 + 开关与判据参数 + 诊断账面。
#[derive(SystemParam)]
pub(super) struct LodInput<'w> {
    pub(super) pass: Res<'w, LodPass>,
    pub(super) settings: Res<'w, LodSettings>,
    pub(super) diag: ResMut<'w, LodDiagnostics>,
}

/// 准备本帧 GPU LOD 选档录制输入：开关决定 compute dispatch 或 host 档位环填 0（最细
/// 档）。选档每帧按当前相机现算，档位环只是迟滞状态，无需 fallback。诊断：在场行的
/// 档位分布（b8 = 本帧槽两帧前 compute 输出或 host 填 0）。
pub(super) fn create_lod_draw(
    pf: &PassFrame,
    lod: &mut LodInput,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<Option<LodDraw>, BevyError> {
    let bound = pf.bound;
    let dispatch = if lod.settings.enabled && bound > 0 {
        first.log(|| info!(
            "GPU LOD 选档 dispatch 首跑（复用视锥 compute 链）：bound {bound}，\
             组 {}，{MAX_LOD_LEVELS} 档（96B push：view_proj+screen+bound+threshold+hysteresis；\
             b7 变体表按档直址，b8 档位环 = 迟滞状态）",
            bound.div_ceil(64),
        ));
        Some(lod.pass.draw(
            &pf.view_proj,
            pf.screen,
            bound,
            lod.settings.threshold_px,
            lod.settings.hysteresis,
        ))
    } else {
        if bound > 0 {
            fill_u32(instances.lod_level_ring(pf.slot), bound as usize, 0)
                .or_fatal("LOD 档位环全最细档填充失败，渲染链无法继续")?;
        }
        None
    };
    if bound > 0 {
        let levels = read_ring(instances.lod_level_ring(pf.slot), bound as usize * LOD_LEVEL_SIZE as usize)
            .or_fatal("LOD 档位环读数失败，渲染链无法继续")?;
        let mut counts = [0u32; MAX_LOD_LEVELS as usize];
        for slot in pf.live {
            let Some(&level) = levels.get(*slot as usize) else { continue };
            let index = (level as usize).min(counts.len() - 1);
            counts[index] += 1;
        }
        lod.diag.enabled = lod.settings.enabled;
        lod.diag.dispatched = dispatch.is_some();
        lod.diag.bound = bound;
        lod.diag.level_counts = counts;
        lod.diag.rows_compared = pf.live.len();
        lod.diag.marks_frame = pf.slot;
    }
    Ok(dispatch)
}
