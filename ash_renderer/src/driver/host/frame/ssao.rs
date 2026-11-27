//! SSAO 帧编排（10.2.2）：按设置决定是否录这一段，并组装本帧的推送参数。
//!
//! 关档返回 `None`：不录 dispatch、不占时间戳查询、零屏障；合成端仍绑 1×1 白
//! dummy（10.2.4 接线），所以管线与集序始终不变。
//!
//! 参数面（[`SsaoSettings`]）走 push constants，不从 FrameUniforms 走——SSAO 是
//! 独立段，参数变化不该惊动共享帧 UBO 的布局。

use ash::vk;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::render::{DescSet, SsaoFrameDraw, SsaoPipelines, SsaoSettings, SsaoTargets};

/// 上采样双边权重的深度尺度（米）：世界位置差超过这个尺度的邻居权重趋零。
const UPSAMPLE_SIGMA_M: f32 = 0.1;

/// SSAO 取用束：设置 + 两条管线（都恒插入，开关只决定录不录）。
#[derive(SystemParam)]
pub(super) struct SsaoInput<'w> {
    pub(super) settings: Res<'w, SsaoSettings>,
    pub(super) pipelines: Res<'w, SsaoPipelines>,
}

/// SSAO 帧局部状态：只给 IGN 旋转一个逐帧变化的种子。
#[derive(Default)]
pub(super) struct SsaoFrameLocal {
    pub(super) frame_index: u32,
}

/// 组装本帧 SSAO 录制束；关档或目标尚未就绪返回 `None`。
pub(super) fn prepare_ssao(
    targets: &SsaoTargets,
    settings: &SsaoSettings,
    pipelines: &SsaoPipelines,
    slot: usize,
    frame_index: u32,
    frame_set: DescSet,
    gbuffer_set: DescSet,
) -> Option<SsaoFrameDraw> {
    if !settings.enabled {
        return None;
    }
    let full = targets.full_extent();
    let half = targets.half_extent();
    if full.width == 0 || full.height == 0 || half.width == 0 || half.height == 0 {
        return None;
    }
    // 诊断模式（判定线 4）：直接写全分辨率 AO，不经上采样；合成端读同一张全分辨率图。
    let storage_set = if settings.full_res {
        targets.full_res_storage_set(slot)
    } else {
        targets.storage_set(slot)
    };
    Some(SsaoFrameDraw::new(
        pipelines,
        frame_set,
        gbuffer_set,
        storage_set,
        half,
        full,
        ssao_push(full, settings, frame_index),
        upsample_push(full),
        settings.full_res,
    ))
}

/// `PushSsao`（32B）：full_w/full_h、radius_m、bias、intensity、steps、frame、flags。
fn ssao_push(full: vk::Extent2D, settings: &SsaoSettings, frame_index: u32) -> [u8; 32] {
    let mut push = [0u8; 32];
    push[0..4].copy_from_slice(&full.width.to_ne_bytes());
    push[4..8].copy_from_slice(&full.height.to_ne_bytes());
    push[8..12].copy_from_slice(&settings.radius_m.to_ne_bytes());
    push[12..16].copy_from_slice(&settings.bias.to_ne_bytes());
    push[16..20].copy_from_slice(&settings.intensity.to_ne_bytes());
    push[20..24].copy_from_slice(&settings.steps.max(1).to_ne_bytes());
    push[24..28].copy_from_slice(&frame_index.to_ne_bytes());
    // flags bit0 = 全分辨率诊断模式（见 `ssao.wgsl` 的 `PushSsao`）。
    let flags = u32::from(settings.full_res) | (u32::from(settings.freeze_rotation) << 1);
    push[28..32].copy_from_slice(&flags.to_ne_bytes());
    push
}

/// `PushUpsample`（16B）：full_w/full_h、sigma、pad。
fn upsample_push(full: vk::Extent2D) -> [u8; 16] {
    let mut push = [0u8; 16];
    push[0..4].copy_from_slice(&full.width.to_ne_bytes());
    push[4..8].copy_from_slice(&full.height.to_ne_bytes());
    push[8..12].copy_from_slice(&UPSAMPLE_SIGMA_M.to_ne_bytes());
    push
}
