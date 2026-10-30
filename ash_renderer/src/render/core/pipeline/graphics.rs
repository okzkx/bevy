//! 图形管线句柄对：pipeline + pipeline layout + 所属 device，Drop 统一销毁。
//! 各管线类型（场景/全屏/overlay/shadow）只负责建链，持有与拆除收在这里。

use ash::{vk, Device};

use crate::render::core::binding::set_layout::SetLayoutDesc;
use crate::common::error::{Result, VulkanError};

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

    /// 管线句柄（帧录制绑定用）。
    #[must_use]
    pub(crate) fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }

    /// pipeline layout 句柄（push/描述符绑定与管线同源）。
    #[must_use]
    pub(crate) fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }

    /// 录制侧的拷贝投影（帧输入束携带；绑定序校验随行）。
    #[must_use]
    pub fn rec(&self) -> PipelineRef {
        PipelineRef { pipeline: self.pipeline, layout: self.layout, sets: self.sets }
    }
}

/// 管线录制投影（Copy）：句柄本体管生命周期，录制输入束带这个。
#[derive(Clone, Copy)]
pub struct PipelineRef {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub sets: &'static [&'static SetLayoutDesc],
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // 契约边界：不等 GPU，退出排空由 teardown 的 device_wait_idle 先行；
        // pipeline 先于 layout（引用方向），layout 先于它借用的 set layouts
        //（归 BindlessTables/FrameUniforms/InstanceTables/GBufferTargets/ShadowTargets 等，
        // 拆除序由 host 的 remove_resource 顺序保证）
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}
