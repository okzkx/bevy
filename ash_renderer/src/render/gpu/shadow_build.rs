//! 8.3 shadow 命令组装 compute 半边：管线 + 录制。命令环（IndirectCommands
//! ×4）与标记环住在 [`super::super::pipeline::shadow::ShadowTargets`]；本模块
//! 只管"怎么派"。组装语义见 `shadow_build.wgsl`（两枚语义钉的执行面注释）。

use ash::vk;
use bevy::log::info;

use crate::common::error::Result;
use crate::render::Context;

const SHADOW_BUILD_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow_build.spv"));

/// compute push 总长（WGSL `PushBuild` 同源）：count 4B。
pub const SHADOW_BUILD_PUSH_SIZE: u32 = 4;

/// 一帧 shadow 命令组装 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct ShadowBuildDraw {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub push: [u8; SHADOW_BUILD_PUSH_SIZE as usize],
    pub groups: u32,
}

/// compute 管线 + layout（[set2, shadow set] + 4B push）。
#[derive(bevy::prelude::Resource)]
pub struct ShadowBuildPass {
    device: ash::Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl ShadowBuildPass {
    /// 建管线链（与 CullPass/IndirectPass 同款形状）。
    ///
    /// # Errors
    /// shader module / layout / pipeline 创建失败。
    pub fn new(
        ctx: &Context,
        set2_layout: vk::DescriptorSetLayout,
        shadow_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &ctx.device;
        let words: Vec<u32> = SHADOW_BUILD_SPV
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
            .size(SHADOW_BUILD_PUSH_SIZE);
        unsafe {
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[set2_layout, shadow_layout])
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
                    info!(
                        "shadow 命令组装管线就绪: shadow_build compute（[set2, shadow set] + 4B push count）"
                    );
                    Ok(Self { device: ctx.device.clone(), layout, pipeline })
                }
                Err((pipelines, e)) => {
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

    /// 组一帧 dispatch 输入：count 打包进 push，组数取整到 workgroup（64）。
    #[must_use]
    pub fn draw(&self, count: u32) -> ShadowBuildDraw {
        let mut push = [0u8; SHADOW_BUILD_PUSH_SIZE as usize];
        push.copy_from_slice(&count.to_le_bytes());
        ShadowBuildDraw {
            pipeline: self.pipeline,
            layout: self.layout,
            push,
            groups: count.div_ceil(64),
        }
    }
}

impl Drop for ShadowBuildPass {
    fn drop(&mut self) {
        // 契约边界：不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成（与 CullPass 同款）
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

/// 录制一帧 shadow 命令组装 dispatch：绑管线 → 双集 → push → dispatch →
/// 命令环屏障（compute 写 → DRAW_INDIRECT 读，5.3 同款形状——shadow 命令环
/// 是独立 buffer，屏障单独立）。标记读依赖由 cull::record 的预置屏障承担。
pub(crate) fn record(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    shadow_set: vk::DescriptorSet,
    build: &ShadowBuildDraw,
) {
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, build.pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            build.layout,
            0,
            &[set2, shadow_set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            build.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &build.push,
        );
        device.cmd_dispatch(command_buffer, build.groups, 1, 1);
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
