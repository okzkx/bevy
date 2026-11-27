//! SSAO 的两条 compute 管线（10.2）：半分辨率 AO 与深度感知上采样。
//!
//! 两条管线都按「帧 UBO → G-buffer 采样集 → AO storage 集」的固定集序建布局，
//! 各带一段 push（参数面，见 shader 里的 `PushSsao` / `PushUpsample`）；合成 pass
//! 另用 [`SSAO_SAMPLE_SET_DESC`] 读全分辨率 AO，所以 AO 图必须既能 storage 写也能
//! sampled 读（见 `ssao_targets`）。

use ash::{Device, vk};
use bevy::prelude::Resource;

use crate::common::error::Result;
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET_DESC;
use crate::render::resident::tables::frame_uniform_table::FRAME_UNIFORM_SET_DESC;
use crate::render::Context;

use super::ssao_targets::SSAO_STORAGE_SET_DESC;

const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 半分辨率 SSAO：读 G-buffer 深度/法线，写半分辨率 AO。
pub(crate) static SSAO_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ssao",
    shader: &shader_abi::SSAO,
    entries: &[c"cs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &SSAO_STORAGE_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: 32,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 半分辨率 AO → 全分辨率 AO（2×2 象限双边上采样）。
pub(crate) static SSAO_UPSAMPLE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "ssao_upsample",
    shader: &shader_abi::SSAO_UPSAMPLE,
    entries: &[c"cs_main"],
    sets: &[
        &FRAME_UNIFORM_SET_DESC,
        &GBUFFER_SAMPLED_SET_DESC,
        &SSAO_STORAGE_SET_DESC,
    ],
    push: Some(PushRange {
        stages: COMPUTE,
        size: 16,
    }),
    color_formats: &[],
    depth_format: None,
};

/// 两条 SSAO compute 管线（进程级：布局与 shader 都不随 resize 变）。
#[derive(Resource)]
pub struct SsaoPipelines {
    ssao: ComputePipeline,
    upsample: ComputePipeline,
}

impl SsaoPipelines {
    /// # Errors
    /// 管线布局对账或管线创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        Ok(Self {
            ssao: ComputePipeline::new(ctx, &SSAO_PIPELINE, layouts)?,
            upsample: ComputePipeline::new(ctx, &SSAO_UPSAMPLE_PIPELINE, layouts)?,
        })
    }

    /// SSAO 半分辨率 compute 的录制句柄。
    #[must_use]
    pub fn ssao(&self) -> ComputeHandles {
        self.ssao.handles()
    }

    /// 上采样 compute 的录制句柄。
    #[must_use]
    pub fn upsample(&self) -> ComputeHandles {
        self.upsample.handles()
    }
}

/// 一帧的 SSAO 录制束：两段 dispatch 的句柄、三个描述符集与两段 push。
///
/// 关档时不存在于 [`RenderFeatures`](crate::render::RenderFeatures) 里（`None`），
/// 所以「关档零命令、零屏障」是类型上就成立的，不靠运行时 if。
pub struct SsaoFrameDraw {
    ssao: ComputeHandles,
    upsample: ComputeHandles,
    frame_set: DescSet,
    gbuffer_set: DescSet,
    storage_set: DescSet,
    half: vk::Extent2D,
    full: vk::Extent2D,
    ssao_push: [u8; 32],
    upsample_push: [u8; 16],
    /// 全分辨率诊断模式：只录一段（SSAO 直接写全分辨率），跳过上采样（判定线 4 对照用）。
    full_res: bool,
}

impl SsaoFrameDraw {
    #[expect(
        clippy::too_many_arguments,
        reason = "逐项对应一帧的绑定与两段 push 字节，成组打包反而要再造一个与录制无关的结构"
    )]
    pub(crate) fn new(
        pipelines: &SsaoPipelines,
        frame_set: DescSet,
        gbuffer_set: DescSet,
        storage_set: DescSet,
        half: vk::Extent2D,
        full: vk::Extent2D,
        ssao_push: [u8; 32],
        upsample_push: [u8; 16],
        full_res: bool,
    ) -> Self {
        Self {
            ssao: pipelines.ssao(),
            upsample: pipelines.upsample(),
            frame_set,
            gbuffer_set,
            storage_set,
            half,
            full,
            ssao_push,
            upsample_push,
            full_res,
        }
    }

    /// 录两段 dispatch：半分辨率 AO（写）→ 全分辨率 AO（读半写全）。
    ///
    /// 断点屏障：第一段末尾把半分辨率 AO 从「compute 写」转成「compute 读写」，
    /// 第二段末尾把全分辨率 AO 转成「compute|片元读」，供合成 pass 的 `textureLoad`。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        // 全分辨率诊断模式：SSAO 直接写全分辨率 AO，没有第二段 dispatch——对照口径
        // 里「全分辨率参照」不该再被一次上采样改动。
        let targets = [self.frame_set, self.gbuffer_set, self.storage_set];
        if self.full_res {
            self.ssao.record_grid(
                device,
                cb,
                &targets,
                &self.ssao_push,
                groups_of(self.full),
                Handoff::ComputeFragmentRead,
            );
            return;
        }
        self.ssao.record_grid(
            device,
            cb,
            &targets,
            &self.ssao_push,
            groups_of(self.half),
            Handoff::ComputeReadWrite,
        );
        self.upsample.record_grid(
            device,
            cb,
            &[self.frame_set, self.gbuffer_set, self.storage_set],
            &self.upsample_push,
            groups_of(self.full),
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
