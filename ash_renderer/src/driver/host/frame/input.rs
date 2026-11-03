//! draw_frame 的参数束：按特性分子束的声明与其写入方法。束只声明"系统要什么资源"，
//! 编排逻辑在 [`super::draw_frame`]；束上的方法只做本帧槽写入（`write_ubo`/`write_slot`
//! 族，调用时该槽 fence 已等过）。

use bevy::{
    ecs::system::SystemParam,
    prelude::*,
    window::WindowResized,
};

use crate::{
    scene::GapSnapshot,
    render::{
        BindlessTextureTable, Context, FramePool, FrameUniformTable,
        GpuTimer,
        PerfLedger,
        Swapchain, Uploader,
    },
};

use super::cluster::ClusterInput;
use super::culling::{FrustumCullInput, OcclusionGate, OcclusionInput};
use super::indirect::IndirectInput;
use super::lod::LodInput;
use super::overlay::OverlayInput;
use super::post::PostInput;
use super::scene::SceneInput;
use super::shadow::ShadowInput;
use super::transparent::TransparentInput;
use super::frame_logs::FrameLogs;
use super::resize_gate::ResizeGate;

/// draw_frame 的参数束：按特性分子束，系统签名只剩本束 + 主线程标记。
#[derive(SystemParam)]
pub(crate) struct FrameInput<'w, 's> {
    pub(super) gpu: FrameGpuInput<'w, 's>,
    pub(super) timing: FrameTimingInput<'w>,
    pub(super) scene: SceneInput<'w, 's>,
    pub(super) frustum_cull: FrustumCullInput<'w>,
    pub(super) indirect: IndirectInput<'w>,
    pub(super) occlusion: OcclusionInput<'w>,
    pub(super) lod: LodInput<'w>,
    pub(super) shadow: ShadowInput<'w>,
    pub(super) post: PostInput<'w>,
    pub(super) cluster: ClusterInput<'w>,
    pub(super) transparent: TransparentInput<'w>,
    /// GPU 缺口快照：帧起在 fence 保护内读缺口标记环的结果，驻留管理器 tick（Last，帧
    /// 提交后）与 BRP census 经此消费，不直读环。
    pub(super) gap_snapshot: ResMut<'w, GapSnapshot>,
    pub(super) overlay: OverlayInput<'w, 's>,
    pub(super) gate: Local<'s, ResizeGate>,
    pub(super) logs: Local<'s, FrameLogs>,
    pub(super) occlusion_gate: Local<'s, OcclusionGate>,
}

/// 帧共享的设备、帧槽、描述符表与上传通道取用束。
#[derive(SystemParam)]
pub(super) struct FrameGpuInput<'w, 's> {
    pub(super) ctx: Res<'w, Context>,
    pub(super) swapchain: ResMut<'w, Swapchain>,
    pub(super) frames: ResMut<'w, FramePool>,
    pub(super) tables: ResMut<'w, BindlessTextureTable>,
    pub(super) frame_uniform_table: ResMut<'w, FrameUniformTable>,
    pub(super) resized: MessageReader<'w, 's, WindowResized>,
    /// 普通帧只读票号，overlay 在场时还会提交图集上传。
    pub(super) uploader: ResMut<'w, Uploader>,
}

/// GPU 时戳与 CPU 帧样本账的取用束。
#[derive(SystemParam)]
pub(super) struct FrameTimingInput<'w> {
    pub(super) timer: ResMut<'w, GpuTimer>,
    pub(super) perf: ResMut<'w, PerfLedger>,
}
