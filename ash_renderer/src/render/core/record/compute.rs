//! compute 录制：可拷贝的管线句柄（绑定、push、一维 dispatch）和 dispatch 后的交接屏障。
//! 各 pass 只保留 push 打包与选哪种交接屏障。

use ash::{vk, Device};

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
