//! compute 管线标准件：shader module 创建、单入口 compute 管线（layout 按布局
//! 配方组装，见 `pipeline_layout`）及其录制助手。gpu/ 下各 pass、
//! Hi-Z 归约与透明层 compute 共用；各 pass 只保留 push 打包与交接屏障形状。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};

use super::pipeline_layout::PipelineDesc;
use super::set_layout::SetLayouts;

/// SPIR-V 字节（build.rs 产物，spirv-val 已过）→ shader module。字节按小端
/// 4B 一字重组（`include_bytes!` 不保证 u32 对齐，不能直接转型）。
///
/// # Errors
/// `vkCreateShaderModule` 失败。
pub(crate) fn create_shader_module(device: &Device, spv: &[u8]) -> Result<vk::ShaderModule> {
    let words: Vec<u32> = spv.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
    unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None) }
        .map_err(VulkanError::from)
}

/// compute 管线的录制句柄（Copy）：Draw 输入束带它进录制层，录制不需要
/// 持有方 Resource。
#[derive(Clone, Copy)]
pub struct ComputeHandles {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
}

/// dispatch 后的交接屏障形状：src 恒为 COMPUTE_SHADER + SHADER_WRITE 的全局
/// 内存屏障，变体只决定下游消费者（dst stage + dst access）。
#[derive(Clone, Copy)]
pub(crate) enum Handoff {
    /// 图形 VS 读或后续 compute 读写（同 buffer 的后续写要 WAW 依赖）。
    VertexComputeReadWrite,
    /// 图形 VS 读或后续 compute 读。
    VertexComputeRead,
    /// 后续 compute 读写。
    ComputeReadWrite,
    /// 后续 compute 读。
    ComputeRead,
    /// 后续 compute 读或透明 FS 读。
    ComputeFragmentRead,
    /// `cmd_draw_indexed_indirect` 读命令环（DRAW_INDIRECT + INDIRECT_COMMAND_READ）。
    IndirectCommandRead,
}

impl Handoff {
    fn dst(self) -> (vk::PipelineStageFlags, vk::AccessFlags) {
        use vk::{AccessFlags as A, PipelineStageFlags as S};
        match self {
            Self::VertexComputeReadWrite => {
                (S::VERTEX_SHADER | S::COMPUTE_SHADER, A::SHADER_READ | A::SHADER_WRITE)
            }
            Self::VertexComputeRead => (S::VERTEX_SHADER | S::COMPUTE_SHADER, A::SHADER_READ),
            Self::ComputeReadWrite => (S::COMPUTE_SHADER, A::SHADER_READ | A::SHADER_WRITE),
            Self::ComputeRead => (S::COMPUTE_SHADER, A::SHADER_READ),
            Self::ComputeFragmentRead => (S::COMPUTE_SHADER | S::FRAGMENT_SHADER, A::SHADER_READ),
            Self::IndirectCommandRead => (S::DRAW_INDIRECT, A::INDIRECT_COMMAND_READ),
        }
    }
}

impl ComputeHandles {
    /// 绑 compute 管线 + 从 set 0 起连续绑 `sets`。
    pub(crate) fn bind(&self, device: &Device, cb: vk::CommandBuffer, sets: &[vk::DescriptorSet]) {
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, self.layout, 0, sets, &[]);
        }
    }

    /// push（`push` 为空 = 管线无 push range，跳过）→ 一维 dispatch → 交接屏障。
    pub(crate) fn dispatch(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        push: &[u8],
        groups: u32,
        handoff: Handoff,
    ) {
        let (dst_stage, dst_access) = handoff.dst();
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(dst_access);
        unsafe {
            if !push.is_empty() {
                device.cmd_push_constants(cb, self.layout, vk::ShaderStageFlags::COMPUTE, 0, push);
            }
            device.cmd_dispatch(cb, groups, 1, 1);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                dst_stage,
                vk::DependencyFlags::empty(),
                std::slice::from_ref(&barrier),
                &[],
                &[],
            );
        }
    }

    /// 单次 dispatch 的完整录制：[`Self::bind`] + [`Self::dispatch`]。
    pub(crate) fn record(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        sets: &[vk::DescriptorSet],
        push: &[u8],
        groups: u32,
        handoff: Handoff,
    ) {
        self.bind(device, cb, sets);
        self.dispatch(device, cb, push, groups, handoff);
    }
}

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
    pub(crate) fn new(device: &Device, desc: &PipelineDesc, layouts: &SetLayouts) -> Result<Self> {
        desc.build(device, layouts, |layout, stages| {
            let &[stage] = stages else {
                return Err(VulkanError::Init(format!("compute 管线 {} 须恰好一个入口", desc.label)));
            };
            let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
            match unsafe { device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) } {
                Ok(mut pipelines) => pipelines
                    .pop()
                    .map(|pipeline| Self { device: device.clone(), handles: ComputeHandles { pipeline, layout } })
                    .ok_or_else(|| {
                        VulkanError::Init(format!("compute 管线 {} 创建成功却未返回句柄", desc.label))
                    }),
                Err((pipelines, e)) => {
                    for p in pipelines {
                        unsafe { device.destroy_pipeline(p, None) };
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
