//! indirect 闭环 compute 半边：命令组装管线 + 录制 + 诊断账面 + CPU 命令
//! 参照。命令环（`BufferRole::IndirectCommands`）与在场环（第五/第六族）住在
//! [`InstanceTables`]；本模块只管"怎么组装、怎么派、怎么计量"。
//!
//! 机制：
//!
//! - **独立组装 pass**：cull（产标记）与 build_indirect（产命令）两步 compute
//!   同构于 Bevy `mesh_preprocess` → `build_indirect_params` 的分工——写职责
//!   不挤一个 shader，cull 管线布局不动（组号随管线布局，改布局要连带重排）。
//! - **在场环**：固定槽位全 bound 发命令会让"已 despawn 未复用槽"与"资产卸载
//!   后暂缓行"的旧记录继续画（幽灵对象）——命令宇宙不是全 bound 槽，而是
//!   DrawList 槽位枚举（在场环）。CPU 槽位枚举是账本在账信息（走组装常驻环
//!   写侧），可见性判断仍在 GPU（marks → instanceCount），红线不碰。
//! - **count 形状**：`cmd_draw_indexed_indirect`（CPU 传在场行数上界），只启用
//!   `multi_draw_indirect`；`draw_indirect_count` 位不启用——翻案条件 = 出现
//!   "实际命令条数只有 GPU 知道"的形状（压缩合批、规模化）。
//! - **push 4B 退役**：firstInstance 携带实例槽号，图形 VS 按
//!   `@builtin(instance_index)` 读 set2，图形 push range 归零。instanceCount
//!   恒 1/0，instance_index == firstInstance == 槽号。
//! - **屏障**：dispatch 后一条 compute 写 → DRAW_INDIRECT 读的内存屏障——
//!   预置屏障只盖标记的 compute/vertex 读，indirect 命令消费走
//!   `DRAW_INDIRECT` 阶段 + `INDIRECT_COMMAND_READ` 访问，须单独立一条
//!   （形状见 [`record`]）。
//! - **首跑留痕**：首个在场行数 > 0 的帧收账一次，VUID 由验证层执法
//!   （不做一次性探针脚手架）。

use ash::vk;
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::bytes::put_u32;
use crate::common::error::{Result, VulkanError};

use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::resident::instance_table::INSTANCE_SET_DESC;
use crate::render::Context;

/// shader 模块字节（build.rs 产物，OUT_DIR 之下；compute 编译链与图形同一闸门）。
pub(crate) static BUILD_INDIRECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "build_indirect",
    shader: &shader_abi::BUILD_INDIRECT,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC],
    push: Some(PushRange { stages: vk::ShaderStageFlags::COMPUTE, size: INDIRECT_PUSH_SIZE }),
    color_formats: &[],
    depth_format: None,
};

/// compute push 总长（pipeline layout 声明 + WGSL `PushBuild` 同源）：
/// count（u32）4B。规范保证 maxPushConstantsSize ≥ 128，无需查询。
pub(crate) const INDIRECT_PUSH_SIZE: u32 = 4;

/// [`IndirectStats`] 的诊断读数（每帧 draw_frame 刷新，egui 面板/BRP 读数口；
/// 只进计量，任何系统不得读它决定画什么——诊断性回读边界同剔除诊断）。
/// 命令内容为 DEVICE_LOCAL 宿主不可读，计数全部由 CPU 在账信息推得：
/// 命令条数 = 在场行数，instanceCount=0 条数 = 在场行数 − 可见标记数。
#[derive(Resource, Default)]
pub struct IndirectStats {
    /// 本帧录制层的场景绘制 API 调用数（恒 1 或 0——判定线 1 的直接读数）。
    pub draw_calls_recorded: u32,
    /// GPU 侧命令条数（= 本帧在场行数）。
    pub command_count: u32,
    /// instanceCount=0 的命令条数（不可见行；诊断标注：不驱动绘制）。
    pub zero_instance_count: u32,
    /// 账本走访行数（O(N) 走访仍在，如实计量不藏——判定线 1 只封绘制
    /// 调用数与 push 字节）。
    pub walk_rows: usize,
    /// 暂缓行数（与 DrawList 语义一致：不在场环 = 不画）。
    pub pending_rows: usize,
}

/// 一帧命令组装 dispatch 的录制输入（draw_frame 组装，[`record`] 消费）。
#[derive(Clone, Copy)]
pub struct IndirectDraw {
    pub pass: ComputeHandles,
    /// 4B push：在场行数（[`IndirectPass::draw`] 打包）。
    pub push: [u8; INDIRECT_PUSH_SIZE as usize],
    /// dispatch 组数 = 在场行数取整到 workgroup（64）。
    pub groups: u32,
}

/// 命令组装 compute 管线（set2 单集 + 4B push）+ count 上限留痕。
#[derive(bevy::prelude::Resource)]
pub struct IndirectPass {
    pipeline: ComputePipeline,
    /// `limits.maxDrawIndirectCount`：drawCount 的硬上界，超界显式报错不截断
    /// （与对象表容量耗尽同纪律）。
    max_draw_indirect_count: u32,
}

impl IndirectPass {
    /// 建管线：layout 只借 set2 + COMPUTE push range。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let pipeline = ComputePipeline::new(ctx, &BUILD_INDIRECT_PIPELINE, layouts)?;
        let max_draw_indirect_count = ctx.limits().max_draw_indirect_count;
        info!(
            "indirect 组装管线就绪: build_indirect compute（set2 + 4B push count）,\
             maxDrawIndirectCount {max_draw_indirect_count}（drawCount 上界，超界显式报错）"
        );
        Ok(Self { pipeline, max_draw_indirect_count })
    }

    /// 组一帧 dispatch 输入：count 打包进 push，组数 = count 取整到 workgroup
    ///（64）。在场行数超 `maxDrawIndirectCount` 显式报错（上界执法）。
    /// draw_frame 消费，录制走 [`record`]。
    ///
    /// # Errors
    /// 在场行数超出设备 `maxDrawIndirectCount`。
    pub fn draw(&self, count: u32) -> Result<IndirectDraw> {
        if count > self.max_draw_indirect_count {
            return Err(VulkanError::Upload(format!(
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

/// 录制一帧命令组装 dispatch（`record_frame` 在剔除 dispatch 之后调用）：绑
/// compute 管线 → set2 → push（count）→ dispatch → 命令环屏障（compute 写 →
/// DRAW_INDIRECT 读）。标记读依赖由 cull::record 的屏障承担（marks 的
/// compute 写 → 本 pass 的 compute 读在其 dst 集合内），此处只收口命令环——
/// 标记屏障不盖 indirect 命令消费，这条必须单独立。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: DescSet,
    indirect: &IndirectDraw,
) {
    indirect.pass.record(
        device,
        command_buffer,
        &[set2],
        &indirect.push,
        indirect.groups,
        Handoff::IndirectCommandRead,
    );
}

#[cfg(test)]
mod tests {
    use super::INDIRECT_PUSH_SIZE;

    /// push 打包形状：count 占 0..4（与 WGSL `PushBuild` 单字段同源）。
    #[test]
    fn push_layout_is_bare_count() {
        assert_eq!(INDIRECT_PUSH_SIZE, 4);
        let mut push = [0u8; INDIRECT_PUSH_SIZE as usize];
        push.copy_from_slice(&77u32.to_le_bytes());
        assert_eq!(u32::from_le_bytes(push), 77);
    }

    /// `IndirectCommand` 字节布局与 `VkDrawIndexedIndirectCommand` 的镜像断言
    ///（20B，五个 u32 字段序 = indexCount/instanceCount/firstIndex/
    /// vertexOffset/firstInstance）——WGSL 侧结构同序互证（build_indirect.wgsl）。
    #[test]
    fn indirect_command_layout_mirror() {
        // 字节序按 wgsl 手排的参照命令（CPU 参照组装语义）：
        // index_count=99, instance_count=1, first_index=7, vertex_offset=11,
        // first_instance=42
        let record_pins = (7u32, 99u32, 11u32); // (first_index, index_count, vertex_base)
        let mark = 1u32;
        let slot = 42u32;
        let mut cmd = [0u32; 5];
        cmd[0] = record_pins.1; // indexCount
        cmd[1] = mark; // instanceCount
        cmd[2] = record_pins.0; // firstIndex
        cmd[3] = record_pins.2; // vertexOffset（位型 i32，值域正）
        cmd[4] = slot; // firstInstance
        let bytes: Vec<u8> = cmd.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(bytes.len(), 20);
        let read_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_u32(0), 99);
        assert_eq!(read_u32(4), 1);
        assert_eq!(read_u32(8), 7);
        assert_eq!(read_u32(12), 11);
        assert_eq!(read_u32(16), 42);
    }
}
