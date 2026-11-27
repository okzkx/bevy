//! 屏幕空间反射的帧编排（10.3.3–10.3.4）：按设置决定录不录 ④.7 段，并打包 48 B push。
//!
//! 三个条件任一不满足就返回 `None`（整段跳过，零命令零屏障）：
//! - 设置里没开；
//! - 当前是 Lambert 以外的调试视图（判定线 1b：`mode 10` 等视图逐字节不变，反射不叠在调试图上）；
//! - 尺寸为 0（最小化）。
//!
//! 参数面（[`ReflectionSettings`]）走 push constants，不进共享帧 UBO——SSR 是独立段，
//! 参数变化不该动 `FRAME_UBO_SIZE` 那套三处同步的契约。

use ash::vk;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::render::{
    DescSet, ReflectionCompositePipeline, ReflectionCopyPipeline, ReflectionFrameDraw,
    ReflectionSettings, ReflectionTargets, RenderMode, SsrFrameDraw, SsrPipelines,
};

/// 步进 push 的字节数（与 `shaders/scene/reflection/ssr.wgsl` 的 `PushSsr` 同形）。
const PUSH_BYTES: usize = 48;

/// 反射取用束：设置 + 三条管线（都恒插入，开关只决定录不录）。
#[derive(SystemParam)]
pub(super) struct ReflectionInput<'w> {
    pub(super) settings: Res<'w, ReflectionSettings>,
    pub(super) copy: Res<'w, ReflectionCopyPipeline>,
    pub(super) pipelines: Res<'w, SsrPipelines>,
    pub(super) composite: Res<'w, ReflectionCompositePipeline>,
}

/// 组装本帧反射录制束；关档 / 调试视图 / 尺寸为零返回 `None`。
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_reflection(
    targets: &ReflectionTargets,
    input: &ReflectionInput,
    mode: RenderMode,
    slot: usize,
    extent: vk::Extent2D,
    hdr_image: vk::Image,
    hdr_view: vk::ImageView,
    frame_set: DescSet,
    gbuffer_set: DescSet,
    ibl_set: DescSet,
) -> Option<ReflectionFrameDraw> {
    if !input.settings.enabled {
        return None;
    }
    // 调试视图（mode 1–12）不叠反射：这些 view 逐字节等于 10.2 的读数
    if mode != RenderMode::Lambert {
        return None;
    }
    let half = targets.half_extent();
    if extent.width == 0 || extent.height == 0 || half.width == 0 || half.height == 0 {
        return None;
    }
    let copy = targets.copy_target(slot);
    let march = SsrFrameDraw::new(
        &input.pipelines,
        frame_set,
        gbuffer_set,
        targets.storage_set(slot),
        half,
        reflection_push(extent, half, &input.settings),
    );
    Some(ReflectionFrameDraw::new(
        march,
        hdr_image,
        hdr_view,
        copy.image(),
        copy.view(),
        extent,
        input.copy.rec(),
        input.composite.rec(),
        gbuffer_set,
        [frame_set, gbuffer_set, ibl_set, targets.sample_set(slot)],
    ))
}

/// 打包 `PushSsr`：4×u32 尺寸、u32 步数、u32 填充、5×f32 参数、u32 填充 = 48 B。
fn reflection_push(
    full: vk::Extent2D,
    half: vk::Extent2D,
    settings: &ReflectionSettings,
) -> [u8; PUSH_BYTES] {
    let mut out = [0u8; PUSH_BYTES];
    let words: [u32; 6] = [
        full.width,
        full.height,
        half.width,
        half.height,
        settings.steps.max(1),
        0,
    ];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
    }
    let floats: [f32; 5] = [
        settings.max_distance_m.max(0.05),
        settings.thickness_m.max(0.0),
        settings.roughness_cutoff.clamp(0.0, 1.0),
        settings.edge_fade.clamp(0.0, 1.0),
        settings.intensity.max(0.0),
    ];
    for (i, f) in floats.iter().enumerate() {
        out[24 + i * 4..28 + i * 4].copy_from_slice(&f.to_ne_bytes());
    }
    out
}
