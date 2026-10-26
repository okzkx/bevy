//! shadow 命令组装 compute 半边：管线 + 录制。命令环（IndirectCommands
//! ×4）与标记环住在 [`super::super::pipeline::shadow::ShadowTargets`]；本模块
//! 只管"怎么派"。组装语义见 `shadow_build.wgsl`（两枚语义钉的执行面注释）。

use ash::vk;
use bevy::log::info;

use crate::common::bytes::put_u32;
use crate::common::error::Result;
use crate::render::gpu::compute::{ComputeHandles, ComputePipeline, Handoff};
use crate::render::Context;

const SHADOW_BUILD_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow_build.spv"));

/// compute push 总长（WGSL `PushBuild` 同源）：count 4B。
pub const SHADOW_BUILD_PUSH_SIZE: u32 = 4;

/// 一帧 shadow 命令组装 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct ShadowBuildDraw {
    pub pass: ComputeHandles,
    pub push: [u8; SHADOW_BUILD_PUSH_SIZE as usize],
    pub groups: u32,
}

/// shadow 命令组装 compute 管线（[set2, shadow set] + 4B push）。
#[derive(bevy::prelude::Resource)]
pub struct ShadowBuildPass {
    pipeline: ComputePipeline,
}

impl ShadowBuildPass {
    /// 建管线：layout 借用 set2 与 shadow set 布局。
    ///
    /// # Errors
    /// shader module / layout / pipeline 创建失败。
    pub fn new(
        ctx: &Context,
        set2_layout: vk::DescriptorSetLayout,
        shadow_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let pipeline = ComputePipeline::new(
            &ctx.device,
            SHADOW_BUILD_SPV,
            &[set2_layout, shadow_layout],
            SHADOW_BUILD_PUSH_SIZE,
            c"cs_main",
        )?;
        info!("shadow 命令组装管线就绪: shadow_build compute（[set2, shadow set] + 4B push count）");
        Ok(Self { pipeline })
    }

    /// 组一帧 dispatch 输入：count 打包进 push，组数取整到 workgroup（64）。
    #[must_use]
    pub fn draw(&self, count: u32) -> ShadowBuildDraw {
        let mut push = [0u8; SHADOW_BUILD_PUSH_SIZE as usize];
        put_u32(&mut push, 0, count);
        ShadowBuildDraw {
            pass: self.pipeline.handles(),
            push,
            groups: count.div_ceil(64),
        }
    }
}

/// 录制一帧 shadow 命令组装 dispatch：绑管线 → 双集 → push → dispatch →
/// 命令环屏障（compute 写 → DRAW_INDIRECT 读——shadow 命令环是独立 buffer，
/// 屏障单独立）。标记读依赖由 cull::record 的屏障承担。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    shadow_set: vk::DescriptorSet,
    build: &ShadowBuildDraw,
) {
    build.pass.record(
        device,
        command_buffer,
        &[set2, shadow_set],
        &build.push,
        build.groups,
        Handoff::IndirectCommandRead,
    );
}
