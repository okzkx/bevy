//! resize 单元：吸收窗口 resize 消息、裁决"本帧整帧让路"还是"帧首按需重建"
//!（ResizeGate），以及尺寸相关资源的重建施工（rebuild_size_dependent）。
//! `draw_frame` 的 `Local`，跨帧保持；frame 只经方法交互，不碰字段。

use bevy::{prelude::*, window::WindowResized};

use crate::{
    common::syntax::{OptionTierExt, ResultTierExt},
    render::{Context, HizPyramid, MAX_FRAMES_IN_FLIGHT},
};

use super::{cluster::ClusterInput, input::FrameGpuInput, post::PostInput};

/// 坑：Windows 最小化即发 `Resized(0,0)`，此后对旧 swapchain 的 `acquire` 无限阻塞，
/// 而阻塞点在 runner 的 `app.update()` 里——消息泵冻死，任务栏"还原"永远点不到。
/// 对策：最小化整帧让路，不碰 Vulkan。
/// 坑：FIFO present mode 下 acquire 在显示队列满/表面失配时会阻塞，拖拽中尤甚；
/// MAILBOX 下 present 直接替换未上屏帧，acquire 不排队。
///
/// 尺寸变化只标 pending、帧首按需重建——acquire 永远落在新 swapchain 上，拖拽中帧循环
/// 持续流动（宁可渲染卡顿，不要冻结）。
#[derive(Default)]
pub(super) struct ResizeGate {
    /// 最新一条 resize 消息是 (0,0) = 窗口最小化中
    minimized: bool,
    /// 尺寸变了 / 驱动报次优，下个帧首按需重建（多设无害，rebuild 幂等）
    pending: bool,
}

impl ResizeGate {
    /// 吸收本帧 resize 消息；`Err(Warning)` = 最小化中、本帧整帧让路——调用方一行
    /// `?` 冒泡统一错误通道，tier_handler 打 warn 后丢弃，下帧继续。
    ///
    /// 一次 update 可能积压多条消息（拖拽 125Hz 输入 vs 60fps 帧），逐条刷状态，
    /// 以最新一条为准。
    pub(super) fn yield_minimized(&mut self, resized: &mut MessageReader<WindowResized>) -> Result<(), BevyError> {
        for msg in resized.read() {
            if msg.width == 0.0 || msg.height == 0.0 {
                self.minimized = true;
            } else {
                self.minimized = false;
                self.pending = true;
            }
        }
        (!self.minimized).then_some(()).ok_or_warn(
            "窗口最小化中，帧循环整帧让路（不碰 swapchain），消息泵保持存活",
        )
    }

    /// 标记"尺寸相关资源待重建"（驱动报次优 / acquire 或 present 过时）。
    /// 多设无害，帧首 rebuild 幂等。
    pub(super) fn request_rebuild(&mut self) {
        self.pending = true;
    }

    /// pending 帧首按需重建（宿主 resize 消息 / swapchain 次优或过时都算）：
    /// 重建全部成功才清，失败原样保留，下个帧首重试。
    pub(super) fn rebuild_if_pending(
        &mut self,
        swapchain_pending: bool,
        rebuild: impl FnOnce() -> Result<(), BevyError>,
    ) -> Result<(), BevyError> {
        if !self.pending && !swapchain_pending {
            return Ok(());
        }
        rebuild()?;
        self.pending = false;
        Ok(())
    }
}

/// 尺寸变化 → 帧首按需重建，时机在 acquire 之前。rebuild 幂等（尺寸没变就空手而归），
/// 拖拽中每步一建、帧循环不断流。其余尺寸相关资源随其后按新 extent 重建（同样幂等；
/// 各自带 device_wait_idle，不会引用到在飞旧资源）。swapchain 重建失败让路下帧重试，
/// 其余失败致命退出。
pub(super) fn rebuild_size_dependent(
    gpu: &mut FrameGpuInput,
    hiz: &mut HizPyramid,
    post: &mut PostInput,
    cluster: &mut ClusterInput,
) -> Result<(), BevyError> {
    let ctx: &Context = &gpu.ctx;
    gpu.swapchain
        .rebuild(ctx)
        .or_warn("resize 后重建失败，下帧重试")?;
    let extent = gpu.swapchain.extent;
    gpu.frames
        .rebuild_depth(ctx, extent)
        .or_fatal("深度附件重建失败，渲染链无法继续")?;
    // Hi-Z 金字塔随深度附件同批重建（warmup 重置，遮挡测试让路到金字塔重新就绪）
    hiz.rebuild(ctx, extent, &per_slot(|s| gpu.frames.depth_image(s)))
        .or_fatal("Hi-Z 金字塔重建失败，渲染链无法继续")?;
    post.bloom
        .rebuild(ctx, extent)
        .or_fatal("Bloom 链重建失败，渲染链无法继续")?;
    // 依赖上面已重建的深度视图（G-buffer 采样集 b3）
    post.gbuffer.targets
        .rebuild(ctx, extent, &per_slot(|s| gpu.frames.depth_view(s)))
        .or_fatal("G-buffer 附件重建失败，渲染链无法继续")?;
    // 光源常驻表与尺寸无关不重建，只把它的 buffer 重新绑进新描述符集
    cluster
        .clusters
        .rebuild(ctx, extent, &per_slot(|s| cluster.lights.ring_buffer(s)))
        .or_fatal("光源分簇 cluster 表重建失败，渲染链无法继续")?;
    // tonemap/edge/blend 的 renderArea 与附件尺寸必须同帧对齐
    post.smaa
        .rebuild(ctx, extent)
        .or_fatal("SMAA 三目标重建失败，渲染链无法继续")
}

/// 逐帧槽收集（重建入参：深度 image/view、光源环 buffer）。
fn per_slot<T>(f: impl FnMut(usize) -> T) -> Vec<T> {
    (0..MAX_FRAMES_IN_FLIGHT).map(f).collect()
}
