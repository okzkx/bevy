//! 屏幕空间反射步进（10.3.3）：一条半分辨率 compute 管线 + 一帧的录制束。
//!
//! 集序固定为「帧 UBO → G-buffer 采样集 → 反射 storage 集」；参数面走一段 48 B push
//! （尺寸 + 步数 + 距离/厚度/门控/强度，见 shader 里的 `PushSsr`）。全屏管线没有 push，
//! 所以步进参数只能放这里；合成 pass 不需要参数（大小从 `textureDimensions` 拿，强度与
//! 门控已经乘进反射图的 alpha = w）。
//!
//! 关档时本束不存在于 [`RenderFeatures`](crate::render::RenderFeatures) 里（`None`），
//! 「关档零命令、零屏障」由类型保证，不靠运行时 if。

use ash::{Device, vk};
use bevy::prelude::Resource;

use crate::common::error::Result;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::passes::reflection::targets::REFLECTION_STORAGE_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::Context;

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 步进 compute 的 push 字节数（`PushSsr`：4×u32 尺寸 + u32 步数 + u32 填充 + 5×f32 + 1×填充）。
pub const SSR_PUSH_BYTES: usize = 48;

/// 半分辨率世界空间步进：读 G-buffer 与 HDR 拷贝，写半分辨率反射图。
pub(crate) static SSR_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ssr",
    shader: &shader_abi::SSR,
    entries: &[c"cs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &REFLECTION_STORAGE_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: SSR_PUSH_BYTES as u32,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 步进 compute 管线（进程级：布局与 shader 都不随 resize 变）。
#[derive(Resource)]
pub struct SsrPipelines {
    march: ComputePipeline,
}

impl SsrPipelines {
    /// # Errors
    /// 管线布局对账或管线创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            march: ComputePipeline::new(ctx, &SSR_PIPELINE, layouts)?,
        })
    }

    /// 步进 compute 的录制句柄。
    #[must_use]
    pub fn march(&self) -> ComputeHandles {
        self.march.handles()
    }
}

/// 一帧的步进录制束：dispatch 句柄、三个描述符集与一段 push。
pub struct SsrFrameDraw {
    march: ComputeHandles,
    frame_set: DescSet,
    gbuffer_set: DescSet,
    storage_set: DescSet,
    half: vk::Extent2D,
    push: [u8; SSR_PUSH_BYTES],
}

impl SsrFrameDraw {
    pub(crate) fn new(
        pipelines: &SsrPipelines,
        frame_set: DescSet,
        gbuffer_set: DescSet,
        storage_set: DescSet,
        half: vk::Extent2D,
        push: [u8; SSR_PUSH_BYTES],
    ) -> Self {
        Self {
            march: pipelines.march(),
            frame_set,
            gbuffer_set,
            storage_set,
            half,
            push,
        }
    }

    /// 录一段 dispatch；末尾把反射图从「compute 写」转成「compute|片元读」，供合成 pass 采样。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.march.record_grid(
            device,
            cb,
            &[self.frame_set, self.gbuffer_set, self.storage_set],
            &self.push,
            groups_of(self.half),
            Handoff::ComputeFragmentRead,
        );
    }
}

/// 8×8 workgroup 覆盖给定 extent 需要的 dispatch 组数（至少 1 组）。
fn groups_of(extent: vk::Extent2D) -> (u32, u32) {
    (
        extent.width.div_ceil(8).max(1),
        extent.height.div_ceil(8).max(1),
    )
}
