//! indirect 命令组装的 compute 段：管线、录制、诊断账面。命令环与在场环住在
//! [`crate::render::InstanceTables`]。
//!
//! - **分两步 compute**：cull 产标记、build_indirect 产命令，写职责不挤一个 shader，
//!   cull 管线布局也不用动（组号随管线布局走，改布局要连带重排）。
//! - **在场环**：命令范围不是全部 bound 槽，而是 DrawList 枚举的可绘行。坑：全 bound
//!   发命令会让"已 despawn 未复用槽"与"资产卸载后暂缓行"的旧记录继续画（幽灵对象）。
//!   可见性判断仍在 GPU（marks → instanceCount）。
//! - **命令条数**：`cmd_draw_indexed_indirect`（CPU 传在场行数上界），只启用
//!   `multi_draw_indirect`；条数只有 GPU 知道时（压缩合批）才需要 `draw_indirect_count`。
//! - **图形侧无 push**：firstInstance 携带实例槽号，VS 按 `@builtin(instance_index)`
//!   读 instance_set。instanceCount 恒 1/0，instance_index == firstInstance == 槽号。

use ash::vk;
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::bytes::put_u32;
use crate::common::error::{Result, VulkanError};

use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;
use crate::render::Context;

pub(crate) static BUILD_INDIRECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "build_indirect",
    shader: &shader_abi::BUILD_INDIRECT,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC],
    push: Some(PushRange {
        stages: vk::ShaderStageFlags::COMPUTE,
        size: INDIRECT_PUSH_SIZE,
    }),
    color_formats: &[],
    depth_format: None,
};

/// compute push 总长（pipeline layout 声明 + WGSL `PushBuild` 同源）：
/// count（u32）4B。
const INDIRECT_PUSH_SIZE: u32 = 4;

/// 组装诊断读数（draw_frame 每帧刷新，egui 面板/BRP 读）。只做计量，不得用它决定
/// 画什么。命令内容在 DEVICE_LOCAL，host 读不到，计数全部由 CPU 账本推得：
/// 命令条数 = 在场行数，instanceCount=0 条数 = 在场行数 − 可见标记数。
#[derive(Resource, Default)]
pub struct IndirectStats {
    /// 本帧录制层的场景绘制 API 调用数（恒 1 或 0）。
    pub draw_calls_recorded: u32,
    /// GPU 侧命令条数（= 本帧在场行数）。
    pub command_count: u32,
    /// instanceCount=0 的命令条数（不可见行）。
    pub zero_instance_count: u32,
    /// 账本走访行数（O(N) 走访如实计量）。
    pub walk_rows: usize,
    /// 暂缓行数（与 DrawList 语义一致：不在场环 = 不画）。
    pub pending_rows: usize,
}

/// 一帧命令组装 dispatch 的录制输入（draw_frame 组装，[`record`] 消费）。
#[derive(Clone, Copy)]
pub struct IndirectDraw {
    pub pass: ComputeHandles,
    /// 4B push：在场行数。
    pub push: [u8; INDIRECT_PUSH_SIZE as usize],
    /// dispatch 组数 = 在场行数取整到 workgroup（64）。
    pub groups: u32,
}

/// 命令组装 compute 管线（instance_set 单集 + 4B push）。
#[derive(Resource)]
pub struct IndirectPass {
    pipeline: ComputePipeline,
    /// `limits.maxDrawIndirectCount`：drawCount 的硬上界，超界显式报错不截断。
    max_draw_indirect_count: u32,
}

impl IndirectPass {
    /// 建管线：layout 只借 instance_set + COMPUTE push range。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let pipeline = ComputePipeline::new(ctx, &BUILD_INDIRECT_PIPELINE, layouts)?;
        let max_draw_indirect_count = ctx.limits().max_draw_indirect_count;
        info!(
            "indirect 组装管线就绪: build_indirect compute（instance_set + 4B push count）,\
             maxDrawIndirectCount {max_draw_indirect_count}（drawCount 上界，超界显式报错）"
        );
        Ok(Self {
            pipeline,
            max_draw_indirect_count,
        })
    }

    /// 组一帧 dispatch 输入。draw_frame 消费，录制走 [`record`]。
    ///
    /// # Errors
    /// 在场行数超出设备 `maxDrawIndirectCount`。
    pub fn draw(&self, count: u32) -> Result<IndirectDraw> {
        if count > self.max_draw_indirect_count {
            return Err(VulkanError::Capacity(format!(
                "在场行数 {count} 超出 maxDrawIndirectCount {}——indirect drawCount 硬上界,显式报错不截断",
                self.max_draw_indirect_count
            )));
        }
        let mut push = [0u8; INDIRECT_PUSH_SIZE as usize];
        put_u32(&mut push, 0, count);
        Ok(IndirectDraw {
            pass: self.pipeline.handles(),
            push,
            groups: count.div_ceil(64),
        })
    }
}

impl IndirectDraw {
    /// 录制一帧命令组装 dispatch（`record_frame` 在剔除 dispatch 之后调用）。交接屏障
    /// 只收口命令环（compute 写 → DRAW_INDIRECT 读）；标记读依赖由 CullDraw::record 的屏障
    /// 承担。坑：标记屏障不盖 indirect 命令消费，这条必须单独立。
    pub(crate) fn record(
        &self,
        device: &ash::Device,
        command_buffer: vk::CommandBuffer,
        instance_set: DescSet,
    ) {
        self.pass.record(
            device,
            command_buffer,
            &[instance_set],
            &self.push,
            self.groups,
            Handoff::IndirectCommandRead,
        );
    }
}
