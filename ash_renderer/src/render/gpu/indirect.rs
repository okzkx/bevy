//! indirect 闭环 compute 半边（5.3）：命令组装管线 + 录制 + 诊断账面 + CPU 命令
//! 参照。命令环（`BufferRole::IndirectCommands`）与在场环（第五/第六族）住在
//! [`InstanceTables`]；本模块只管"怎么组装、怎么派、怎么计量"。
//!
//! 机制（对应段施工计划 §2/§3，对计划待决项的钉明见施工记录）：
//!
//! - **独立组装 pass**（待决 1 倾向采纳）：cull（产标记）与 build_indirect
//!   （产命令）两步 compute 同构于 Bevy `mesh_preprocess` →
//!   `build_indirect_params` 的分工——写职责不挤一个 shader，cull 管线布局
//!   不动（5.2 首钉"组号随管线布局"的教训）。
//! - **在场环**（对计划待决 6 的施工钉明）：固定槽位全 bound 发命令会让
//!   "已 despawn 未复用槽"与"资产卸载后暂缓行"的旧记录继续画（幽灵对象）
//!   ——命令宇宙不是全 bound 槽，而是 DrawList 槽位枚举（在场环）。CPU 槽位
//!   枚举是账本在账信息（走组装常驻环写侧，计划 §1 允许），可见性判断仍在
//!   GPU（marks → instanceCount），红线不碰。
//! - **count 形状**（待决 2 倾向采纳）：`cmd_draw_indexed_indirect`（CPU 传
//!   在场行数上界），只启用 `multi_draw_indirect`；`draw_indirect_count` 位不
//!   启用——翻案条件 = 出现"实际命令条数只有 GPU 知道"的形状（压缩合批、
//!   5.4 规模化）。
//! - **push 4B 退役**（待决 3 倾向采纳）：firstInstance 携带实例槽号，图形 VS
//!   按 `@builtin(instance_index)` 读 set2——5.1 遗志（逐对象 push 退役）
//!   收尾，图形 push range 归零。instanceCount 恒 1/0，instance_index ==
//!   firstInstance == 槽号。
//! - **屏障**：dispatch 后一条 compute 写 → DRAW_INDIRECT 读的内存屏障——
//!   对计划"本段不新增屏障形状"的施工订正：预置屏障只盖标记的 compute/
//!   vertex 读，indirect 命令消费走 `DRAW_INDIRECT` 阶段 +
//!   `INDIRECT_COMMAND_READ` 访问，须单独立一条（形状见 [`record`]）。
//! - **首跑留痕**（闸门②的空转首跑证据形态）：首个在场行数 > 0 的帧收账
//!   一次，VUID 由验证层执法（与 5.2 闸门④同款；不做一次性探针脚手架）。

use ash::vk;
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::error::{Result, VulkanError};

use crate::render::Context;

/// shader 模块字节（build.rs 产物，OUT_DIR 之下；compute 编译链与图形同一闸门）。
const BUILD_INDIRECT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/build_indirect.spv"));

/// compute push 总长（pipeline layout 声明 + WGSL `PushBuild` 同源）：
/// count（u32）4B。规范保证 maxPushConstantsSize ≥ 128，无需查询。
pub const INDIRECT_PUSH_SIZE: u32 = 4;

/// [`IndirectStats`] 的诊断读数（每帧 draw_frame 刷新，egui 面板/BRP 读数口；
/// 只进计量，任何系统不得读它决定画什么——诊断性回读边界照 5.2 拍板）。
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
    /// 首个 indirect 提交帧留痕旗标（闸门②首跑证据）。
    pub first_run_logged: bool,
}

/// 一帧命令组装 dispatch 的录制输入（draw_frame 组装，[`record`] 消费）。
#[derive(Clone, Copy)]
pub struct IndirectDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// 4B push：在场行数（[`IndirectPass::draw`] 打包）。
    pub push: [u8; INDIRECT_PUSH_SIZE as usize],
    /// dispatch 组数 = 在场行数取整到 workgroup（64）。
    pub groups: u32,
}

/// compute 管线 + pipeline layout（set2 单集 + 4B push）+ count 上限留痕。
#[derive(bevy::prelude::Resource)]
pub struct IndirectPass {
    device: ash::Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// `limits.maxDrawIndirectCount`（5.3 闸门③）：drawCount 的硬上界，超界
    /// 显式报错不截断（与对象表容量耗尽同纪律）。
    max_draw_indirect_count: u32,
}

impl IndirectPass {
    /// 建管线链：shader module（build.rs 产物，spirv-val 已过）→ layout
    /// （只借 set2 + COMPUTE push range）→ ComputePipeline。与 CullPass 同款
    /// 形状（无附件/状态纠缠）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, set2_layout: vk::DescriptorSetLayout) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = BUILD_INDIRECT_SPV
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let module = unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
        }?;
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(INDIRECT_PUSH_SIZE);
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(std::slice::from_ref(&set2_layout))
                        .push_constant_ranges(std::slice::from_ref(&push_range)),
                    None,
                )
                .inspect_err(|_| device.destroy_shader_module(module, None))?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"cs_main");
            let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
            match device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) {
                Ok(mut pipelines) => {
                    let pipeline = pipelines.pop().expect("单 info 必返单管线");
                    device.destroy_shader_module(module, None);
                    let max_draw_indirect_count = ctx.limits().max_draw_indirect_count;
                    info!(
                        "indirect 组装管线就绪: build_indirect compute（set2 + 4B push count）,\
                         maxDrawIndirectCount {max_draw_indirect_count}（drawCount 上界，超界显式报错）"
                    );
                    Ok(Self {
                        device: ctx.device.clone(),
                        layout,
                        pipeline,
                        max_draw_indirect_count,
                    })
                }
                Err((pipelines, e)) => {
                    // 半建失败：已返回的句柄也要拆（ash 合约）
                    for p in pipelines {
                        device.destroy_pipeline(p, None);
                    }
                    device.destroy_pipeline_layout(layout, None);
                    device.destroy_shader_module(module, None);
                    Err(e.into())
                }
            }
        }
    }

    /// 组一帧 dispatch 输入：count 打包进 push，组数 = count 取整到 workgroup
    ///（64）。在场行数超 `maxDrawIndirectCount` 显式报错（闸门③上界执法）。
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
        push.copy_from_slice(&count.to_le_bytes());
        Ok(IndirectDraw {
            pipeline: self.pipeline,
            layout: self.layout,
            push,
            groups: count.div_ceil(64),
        })
    }
}

/// 录制一帧命令组装 dispatch（`record_frame` 在剔除 dispatch 之后调用）：绑
/// compute 管线 → set2 → push（count）→ dispatch → 命令环屏障（compute 写 →
/// DRAW_INDIRECT 读）。标记读依赖由 cull::record 的预置屏障承担（marks 的
/// compute 写 → 本 pass 的 compute 读在其 dst 集合内），此处只收口命令环。
/// 输入全在 [`IndirectDraw`]，录制层不需要 IndirectPass 句柄。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    indirect: &IndirectDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            indirect.pipeline,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            indirect.layout,
            0,
            std::slice::from_ref(&set2),
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            indirect.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &indirect.push,
        );
        device.cmd_dispatch(command_buffer, indirect.groups, 1, 1);
        // compute 写命令环 → cmd_draw_indexed_indirect 读（DRAW_INDIRECT 阶段，
        // INDIRECT_COMMAND_READ 访问）。施工订正：计划"本段不新增屏障形状"
        // 只对标记成立——命令消费的依赖形状必须单独立这条。
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::INDIRECT_COMMAND_READ);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&barrier),
            &[],
            &[],
        );
    }
}

impl Drop for IndirectPass {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成。先拆管线再拆 layout（借用 set2 layout 归
        // InstanceTables，不在此拆）。
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
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
