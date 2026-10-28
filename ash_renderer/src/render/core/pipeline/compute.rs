//! compute 管线：按布局配方建单入口 compute 管线并持有，录制用的句柄见
//! `record::compute`。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::Context;
use crate::render::core::binding::pipeline_layout::PipelineDesc;
use crate::render::core::binding::set_layout::SetLayouts;
use crate::render::core::record::compute::ComputeHandles;

/// 单入口 compute 管线 + pipeline layout。set layouts 是借用（归各自持有方
/// 拆），Drop 只拆管线与 layout。
pub struct ComputePipeline {
    device: Device,
    handles: ComputeHandles,
}

impl ComputePipeline {
    /// 按配方建单入口 compute 管线（骨架见 [`PipelineDesc::build`]）。半建返回的
    /// 管线逐个回收（ash 合约）。
    ///
    /// # Errors
    /// 配方对账不过、入口不是一个，或 layout / module / pipeline 任一创建失败。
    pub(crate) fn new(ctx: &Context, desc: &PipelineDesc, layouts: &SetLayouts) -> Result<Self> {
        desc.build(ctx, layouts, |layout, stages| {
            let &[stage] = stages else {
                return Err(VulkanError::Init(format!("compute 管线 {} 须恰好一个入口", desc.label)));
            };
            let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
            match unsafe { ctx.device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) } {
                Ok(mut pipelines) => pipelines
                    .pop()
                    .map(|pipeline| Self { device: ctx.device.clone(), handles: ComputeHandles { pipeline, layout, sets: desc.sets } })
                    .ok_or_else(|| {
                        VulkanError::Init(format!("compute 管线 {} 创建成功却未返回句柄", desc.label))
                    }),
                Err((pipelines, e)) => {
                    for p in pipelines {
                        unsafe { ctx.device.destroy_pipeline(p, None) };
                    }
                    Err(e.into())
                }
            }
        })
    }

    /// 录制句柄（Draw 输入束的素材）。
    #[must_use]
    pub fn handles(&self) -> ComputeHandles {
        self.handles
    }
}

impl Drop for ComputePipeline {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成。先拆管线再拆 layout。
        unsafe {
            self.device.destroy_pipeline(self.handles.pipeline, None);
            self.device.destroy_pipeline_layout(self.handles.layout, None);
        }
    }
}
