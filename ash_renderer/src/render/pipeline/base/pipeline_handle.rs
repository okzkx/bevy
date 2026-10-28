//! 图形管线句柄对：pipeline + pipeline layout + 所属 device，Drop 统一销毁。
//! 各管线类型（场景/全屏/overlay/shadow）只负责建链，持有与拆除收在这里。
//! 另含图形管线的录制助手（绑定、viewport/scissor）：所有 `unsafe fn` 的前提与 ash 的 `cmd_*` 相同，
//! `cb` 处于录制状态，传入的句柄合法且活到命令执行完。

use ash::{vk, Device};
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
}

impl PipelineHandle {
    /// 接管刚建成的 `layout` + `pipeline`（此后由本对象负责销毁）。
    pub(crate) fn new(device: &Device, layout: vk::PipelineLayout, pipeline: vk::Pipeline) -> Self {
        Self { device: device.clone(), layout, pipeline }
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
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // 契约边界：不等 GPU，退出排空由 teardown 的 device_wait_idle 先行；
        // pipeline 先于 layout（引用方向），layout 先于它借用的 set layouts
        //（归 BindlessTables/InstanceTables/GBufferTargets/ShadowTargets 等，
        // 拆除序由 host 的 remove_resource 顺序保证）
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

/// 覆盖整个 `extent` 的矩形，用作 renderArea 或 scissor。
pub(crate) fn full_rect(extent: vk::Extent2D) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }
}

/// 全幅 viewport，深度范围 [0,1]。
/// 坑：高度用正值，不要改成负高度翻转 Y——Y 已在 shader 里翻过，再翻画面会倒过来（见 gbuffer_pipeline 模块注释）。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn set_viewport(device: &Device, cb: vk::CommandBuffer, extent: vk::Extent2D) {
    unsafe {
        device.cmd_set_viewport(
            cb,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
    }
}

/// 全幅 viewport 加全幅 scissor。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn set_viewport_scissor(
    device: &Device,
    cb: vk::CommandBuffer,
    extent: vk::Extent2D,
) {
    unsafe {
        set_viewport(device, cb, extent);
        device.cmd_set_scissor(cb, 0, &[full_rect(extent)]);
    }
}

/// 绑图形管线，并从 set 0 起依次绑定 `sets`（无动态偏移）。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn bind_graphics(
    device: &Device,
    cb: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    sets: &[vk::DescriptorSet],
) {
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, layout, 0, sets, &[]);
    }
}

/// 整块绑定顶点/索引 buffer（binding 0、偏移 0、U32 索引）；每个 draw 靠 vertexOffset/firstIndex 定位自己的数据。
///
/// # Safety
/// 见模块注释。
pub(crate) unsafe fn bind_vertex_index(
    device: &Device,
    cb: vk::CommandBuffer,
    vertex: vk::Buffer,
    index: vk::Buffer,
) {
    unsafe {
        device.cmd_bind_vertex_buffers(cb, 0, &[vertex], &[0]);
        device.cmd_bind_index_buffer(cb, index, 0, vk::IndexType::UINT32);
    }
}
