//! 图形管线句柄对：pipeline + pipeline layout + 所属 device，Drop 统一销毁。
//! 各管线类型（场景/全屏/overlay/shadow）只负责建链，持有与拆除收在这里。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::set_layout::SetLayoutDesc;
use crate::render::core::record::graphics::PipelineRef;

/// 建单条图形管线；失败时回收驱动半建返回的句柄。
pub(crate) fn create_graphics_pipeline(
    device: &Device,
    info: &vk::GraphicsPipelineCreateInfo<'_>,
) -> Result<vk::Pipeline> {
    match unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[*info], None) } {
        Ok(mut pipelines) => pipelines
            .pop()
            .ok_or_else(|| VulkanError::Init("单条 graphics pipeline 创建未返回句柄".into())),
        Err((pipelines, error)) => {
            unsafe {
                for pipeline in pipelines {
                    if pipeline != vk::Pipeline::null() {
                        device.destroy_pipeline(pipeline, None);
                    }
                }
            }
            Err(VulkanError::Vk(error))
        }
    }
}

/// 一条已建成的图形管线及其 layout（独占所有权，Drop 销毁两者）。
pub(crate) struct PipelineHandle {
    device: Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// 预期的 set 声明序列（建管线时从配方抄入；绑定序校验用）。
    sets: &'static [&'static SetLayoutDesc],
}

impl PipelineHandle {
    /// 接管刚建成的 `layout` + `pipeline`（此后由本对象负责销毁）。
    pub(crate) fn new(
        device: &Device,
        layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
        sets: &'static [&'static SetLayoutDesc],
    ) -> Self {
        Self { device: device.clone(), layout, pipeline, sets }
    }

    /// 录制句柄（帧输入束携带；绑定序校验随行）。
    #[must_use]
    pub fn rec(&self) -> PipelineRef {
        PipelineRef { pipeline: self.pipeline, layout: self.layout, sets: self.sets }
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 的 device_wait_idle 先行。pipeline 先于 layout
        //（引用方向），layout 先于它借用的 set layouts（归各资源族，拆除序由 host 的
        // remove_resource 顺序保证）
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}
