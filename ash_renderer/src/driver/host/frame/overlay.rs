//! overlay 绘制的帧侧单元：取用束（OverlayInput）与本帧 UI 绘制备料
//!（create_overlay_draw）。镶嵌/图集/顶点环本体在 [`crate::overlay`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use ash::vk;

use crate::{
    overlay::{paint_overlay, OverlayLogState, UiDrawData},
    render::{BindlessTextureTable, Context, OverlayPipeline, UiPaint, Uploader},
};

/// overlay 绘制输入：UI 数据可缺席，管线仍是必需资源。
#[derive(SystemParam)]
pub(super) struct OverlayInput<'w, 's> {
    pub(super) ui: Option<UiDrawData<'w>>,
    pub(super) pipeline: Res<'w, OverlayPipeline>,
    pub(super) paint_log: Local<'s, OverlayLogState>,
}

/// UI 绘制产出：绘制参数 + 该帧槽的顶点/索引环句柄（None = UI 缺席，UI 段整段跳过）。
pub(super) type UiDrawOutput = Option<(UiPaint, (vk::Buffer, vk::Buffer))>;

/// 本帧 UI 绘制备料：图集整传（可能多一张 transfer 票据，本帧 upload_wait 快照必须在此后
/// 取）→ 镶嵌 → 顶点环写入。UI 数据缺席 = None（record_frame 的 UI 段整段跳过）。
pub(super) fn create_overlay_draw(
    ctx: &Context,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    overlay: &mut OverlayInput,
    slot: usize,
    extent: vk::Extent2D,
) -> Result<UiDrawOutput, BevyError> {
    let Some(mut ui_data) = overlay.ui.take() else {
        return Ok(None);
    };
    let ui_buffers = ui_data.ring.buffers(slot);
    let paint = paint_overlay(
        ctx,
        &ui_data.egui,
        &mut ui_data.frame,
        &mut ui_data.mirror,
        &mut ui_data.gpu,
        &mut ui_data.ring,
        tables,
        uploader,
        slot,
        extent,
        &mut overlay.paint_log,
    )?;
    Ok(paint.map(|p| (p, ui_buffers)))
}
