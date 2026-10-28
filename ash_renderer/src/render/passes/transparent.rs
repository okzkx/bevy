//! 透明层：挑出、排序、组装透明物体的绘制命令，再前向画进 HDR 目标。光源读
//! [`crate::render::passes::lighting::cluster`] 的光源分簇集。挑选和排序都在 GPU 上做，CPU 只给在场行数。
//!
//! 每帧顺序：collect 挑出 alpha<1 的行并算深度键 → sort 按深度键排序 →
//! gather 生成间接绘制命令 → 一次 indirect 绘制。
//! 透明绘制把不透明 pass 的深度图重新挂成深度附件，只测不写。坑：挂成附件的
//! 深度图不能同时采样，shader 要读深度就得另拷一份。

use ash::{vk, Device};
use bevy::log::info;
use bevy::prelude::Resource;

use crate::common::bytes::put_u32;
use crate::common::error::{Result, VulkanError};
use crate::render::core::kit::compute_pipeline::{ComputeHandles, ComputePipeline, Handoff};
use crate::render::core::device::memory::{BufferRole, GpuBuffer};
use crate::render::Context;
use crate::render::core::kit::pipeline_handle::create_graphics_pipeline;
use crate::render::core::kit::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::kit::set_layout::{BindingDesc, DescKind, SetLayout, SetLayoutDesc, SetLayouts};
use crate::render::core::kit::shader_abi;
use crate::render::core::kit::storage_sets;
use crate::render::passes::lighting::cluster::LIGHT_CLUSTER_SET;
use crate::render::resident::descriptors::{BINDLESS_SET, FRAME_SET};
use crate::render::resident::instance_table::INSTANCE_SET;

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 透明命令集：b0 索引列表 / b1 深度键 / b2 命令环 / b3 计数，只给 compute。
pub(crate) static TRANSPARENT_SET: SetLayoutDesc = SetLayoutDesc::new(
    "transparent",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, CS),
        BindingDesc::new(1, DescKind::StorageBuffer, CS),
        BindingDesc::new(2, DescKind::StorageBuffer, CS),
        BindingDesc::new(3, DescKind::StorageBuffer, CS),
    ],
);

pub(crate) static TRANSPARENT_COLLECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_collect",
    shader: &shader_abi::TRANSPARENT_COLLECT,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET, &LIGHT_CLUSTER_SET, &TRANSPARENT_SET],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_PUSH_SIZE }),
};

pub(crate) static TRANSPARENT_SORT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_sort",
    shader: &shader_abi::TRANSPARENT_SORT,
    entries: &[c"cs_main"],
    sets: &[&TRANSPARENT_SET],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_SORT_PUSH }),
};

pub(crate) static TRANSPARENT_GATHER_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_gather",
    shader: &shader_abi::TRANSPARENT_GATHER,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET, &TRANSPARENT_SET],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_PUSH_SIZE }),
};

pub(crate) static TRANSPARENT_FORWARD_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_forward",
    shader: &shader_abi::TRANSPARENT_FORWARD,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&BINDLESS_SET, &FRAME_SET, &INSTANCE_SET, &LIGHT_CLUSTER_SET],
    push: None,
};

/// 透明行容量，索引列表、深度键、命令环都按它分配。在场行数超过它时
/// [`TransparentPasses::collect`] 直接报错。
pub const TRANSPARENT_CAPACITY: u32 = 16384;

/// 透明命令链，每帧槽一份。容量固定，resize 不重建。set 布局：b0 索引列表 /
/// b1 深度键 / b2 命令环 / b3 计数，只给 compute 用；绘制直接读命令环 buffer。
#[derive(Resource)]
pub struct TransparentTargets {
    device: Device,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    /// 透明行的实例槽号（显存）。
    index_lists: Vec<GpuBuffer>,
    /// 排序用的视深（f32）。
    keys: Vec<GpuBuffer>,
    /// 间接绘制命令，每条 20B。
    commands: Vec<GpuBuffer>,
    /// 透明行数（CPU 可见）：CPU 每帧清零，collect 原子累加，BRP 直读。
    counts: Vec<GpuBuffer>,
}

impl TransparentTargets {
    /// 建到一半失败时，已建的部分由 Drop 回收。
    ///
    /// # Errors
    /// buffer 创建/内存分配/描述符对象创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            index_lists: Vec::new(),
            keys: Vec::new(),
            commands: Vec::new(),
            counts: Vec::new(),
        };
        let sharing = ctx.sharing_families();
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(&ctx.device, ctx.memory_contract(), size, role, &sharing)
        };
        let capacity = u64::from(TRANSPARENT_CAPACITY);
        let mut slots = Vec::with_capacity(crate::render::MAX_FRAMES_IN_FLIGHT);
        for slot in 0..crate::render::MAX_FRAMES_IN_FLIGHT {
            this.index_lists.push(mk(BufferRole::DeviceStorage, capacity * 4)?);
            this.keys.push(mk(BufferRole::DeviceStorage, capacity * 4)?);
            this.commands.push(mk(BufferRole::IndirectCommands, capacity * 20)?);
            this.counts.push(mk(BufferRole::Storage, 4)?);
            slots.push([
                this.index_lists[slot].buffer(),
                this.keys[slot].buffer(),
                this.commands[slot].buffer(),
                this.counts[slot].buffer(),
            ]);
        }
        this.set_layout = TRANSPARENT_SET.create(&ctx.device)?;
        (this.pool, this.sets) = unsafe { storage_sets::create_sets(&ctx.device, this.set_layout, &slots) }?;
        info!(
            "透明命令链就绪: 透明行容量 {TRANSPARENT_CAPACITY}（索引/键列表 + 命令环 + 计数）× {} 帧槽",
            this.sets.len(),
        );
        Ok(this)
    }

    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&TRANSPARENT_SET, self.set_layout)
    }

    #[must_use]
    pub fn frame_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot % self.sets.len()]
    }

    #[must_use]
    pub fn command_buffer(&self, slot: usize) -> vk::Buffer {
        self.commands[slot % self.commands.len()].buffer()
    }

    /// 第 `slot` 帧槽的深度键 buffer；每帧先填 -inf，空位排序后落在最前面。
    #[must_use]
    pub fn key_buffer(&self, slot: usize) -> vk::Buffer {
        self.keys[slot % self.keys.len()].buffer()
    }

    /// 第 `slot` 帧槽的透明行数。BRP 读到的是该槽上一轮（两帧前）的结果。
    #[must_use]
    pub fn count(&mut self, slot: usize) -> &mut GpuBuffer {
        let n = self.counts.len();
        &mut self.counts[slot % n]
    }

    /// 读回第 `slot` 帧槽的索引列表和深度键，供 BRP 排序对照。每次临时建
    /// staging，只给低频诊断用，不要放进帧循环。
    ///
    /// # Errors
    /// 读回提交失败（设备丢失等）。
    pub fn readback_sort(
        &self,
        ctx: &Context,
        slot: usize,
        indices: &mut [u8],
        keys: &mut [u8],
    ) -> Result<()> {
        crate::render::core::device::readback::readback_buffer(
            ctx,
            self.index_lists[slot % self.index_lists.len()].buffer(),
            indices,
        )?;
        crate::render::core::device::readback::readback_buffer(
            ctx,
            self.keys[slot % self.keys.len()].buffer(),
            keys,
        )
    }
}

impl Drop for TransparentTargets {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown 已先 device_wait_idle；buffer 随字段释放
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout, None);
            }
        }
    }
}

/// collect/gather 的 push 大小：一个 u32 在场行数。
pub const TRANSPARENT_PUSH_SIZE: u32 = 4;

/// 一帧透明层的录制输入。光源分簇集和 cluster_build 不在这里，在 `FrameDraw`
/// 上，因为不透明合成也要用。
pub struct TransparentFrame {
    pub transparent_set: vk::DescriptorSet,
    /// 在场行数 > 0 才 Some。
    pub collect: Option<TransparentCollectDraw>,
    pub gather: Option<TransparentGatherDraw>,
    /// 坑：只要场景非空每帧都要排，透明行从 0 变多的那一帧也不能读未排序的数组。
    pub sort: TransparentSortPlan,
    /// 常驻实例表，gather 和透明绘制都要绑。
    pub set2: vk::DescriptorSet,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub t_commands: vk::Buffer,
    /// 每帧录制前填 -inf。
    pub t_keys: vk::Buffer,
    /// 命令条数 = 在场行数；不是透明的行 instanceCount 为 0。
    pub command_count: u32,
}

/// collect 的一次 dispatch；push = 在场行数。
#[derive(Clone, Copy)]
pub struct TransparentCollectDraw {
    pub pass: ComputeHandles,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// gather 的一次 dispatch；push = 在场行数，shader 用它挡越界线程。
#[derive(Clone, Copy)]
pub struct TransparentGatherDraw {
    pub pass: ComputeHandles,
    pub push: [u8; TRANSPARENT_PUSH_SIZE as usize],
    pub groups: u32,
}

/// sort 的 push 大小：(k, j) 两个 u32。
pub const TRANSPARENT_SORT_PUSH: u32 = 8;

/// 一帧的 bitonic 排序：同一管线按 (k, j) 序列逐次 dispatch。
pub struct TransparentSortPlan {
    pub pass: ComputeHandles,
    /// 每次 dispatch 的线程组数：每线程处理一对元素。
    pub groups: u32,
    /// 105 组 (k, j)。每帧拷一份（840B），让帧输入不借用 `TransparentPasses`。
    pub passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
}

/// 透明层的三条 compute 管线。绑定的 set：collect 绑 set2、光源分簇集、透明集；
/// sort 只绑透明集；gather 绑 set2、透明集。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPasses {
    sort: ComputePipeline,
    /// 容量固定，序列建管线时生成一次。
    sort_passes: Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>,
    collect: ComputePipeline,
    gather: ComputePipeline,
}

impl TransparentPasses {
    /// 中途失败时已建的管线随作用域释放；set 布局归各自资源销毁。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let device = &ctx.device;
        let sort_passes = bitonic_sequence(TRANSPARENT_CAPACITY)?;
        let sort = ComputePipeline::new(device, &TRANSPARENT_SORT_PIPELINE, layouts)?;
        let collect = ComputePipeline::new(device, &TRANSPARENT_COLLECT_PIPELINE, layouts)?;
        let gather = ComputePipeline::new(device, &TRANSPARENT_GATHER_PIPELINE, layouts)?;
        info!(
            "透明层 compute 三件就绪: collect（[set2, cluster, transparent]）/ sort（[transparent]）/ gather（[set2, transparent]）"
        );
        Ok(Self { sort, sort_passes, collect, gather })
    }

    /// # Errors
    /// 在场行数超过 [`TRANSPARENT_CAPACITY`]。
    pub fn collect(&self, count: u32) -> Result<TransparentCollectDraw> {
        if count > TRANSPARENT_CAPACITY {
            return Err(VulkanError::Upload(format!(
                "在场行数 {count} 超出透明行容量 {TRANSPARENT_CAPACITY}——显式报错不截断"
            )));
        }
        let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
        put_u32(&mut push, 0, count);
        Ok(TransparentCollectDraw {
            pass: self.collect.handles(),
            push,
            groups: count.div_ceil(64),
        })
    }

    #[must_use]
    pub fn gather(&self, count: u32) -> TransparentGatherDraw {
        let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
        put_u32(&mut push, 0, count);
        TransparentGatherDraw {
            pass: self.gather.handles(),
            push,
            groups: count.div_ceil(64),
        }
    }

    #[must_use]
    pub fn sort_plan(&self) -> TransparentSortPlan {
        TransparentSortPlan {
            pass: self.sort.handles(),
            groups: (TRANSPARENT_CAPACITY / 2).div_ceil(64),
            passes: self.sort_passes.clone(),
        }
    }
}

/// bitonic 排序的 (k, j) 序列：k 从 2 倍增到 n，每个 k 下 j 从 k/2 减半到 1。
///
/// # Errors
/// n 不是 2 的幂。
fn bitonic_sequence(n: u32) -> Result<Vec<[u8; TRANSPARENT_SORT_PUSH as usize]>> {
    if !n.is_power_of_two() {
        return Err(VulkanError::Init(format!("bitonic 容量 {n} 不是 2 的幂")));
    }
    let mut passes = Vec::new();
    let mut k = 2u32;
    while k <= n {
        let mut j = k / 2;
        while j >= 1 {
            let mut push = [0u8; TRANSPARENT_SORT_PUSH as usize];
            put_u32(&mut push, 0, k);
            put_u32(&mut push, 4, j);
            passes.push(push);
            j /= 2;
        }
        k <<= 1;
    }
    Ok(passes)
}

/// 录制 collect；自带屏障让后面的 compute 读到列表、键和计数。
pub(crate) fn record_transparent_collect(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    cluster_set: vk::DescriptorSet,
    transparent_set: vk::DescriptorSet,
    draw: &TransparentCollectDraw,
) {
    draw.pass.record(
        device,
        command_buffer,
        &[set2, cluster_set, transparent_set],
        &draw.push,
        draw.groups,
        Handoff::ComputeRead,
    );
}

/// 录制排序：只绑一次，逐个 (k, j) dispatch；每次之间加屏障，下一轮才能读到
/// 上一轮的交换结果。
pub(crate) fn record_transparent_sort(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    transparent_set: vk::DescriptorSet,
    plan: &TransparentSortPlan,
) {
    plan.pass.bind(device, command_buffer, &[transparent_set]);
    for pass in &plan.passes {
        plan.pass.dispatch(device, command_buffer, pass, plan.groups, Handoff::ComputeReadWrite);
    }
}

/// 录制 gather；自带屏障让后面的间接绘制读到新命令。
pub(crate) fn record_transparent_gather(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    set2: vk::DescriptorSet,
    transparent_set: vk::DescriptorSet,
    draw: &TransparentGatherDraw,
) {
    draw.pass.record(
        device,
        command_buffer,
        &[set2, transparent_set],
        &draw.push,
        draw.groups,
        Handoff::IndirectCommandRead,
    );
}

/// 透明前向图形管线：alpha 混合画进 HDR 目标，深度只测不写；set 依次为
/// set0/set1/set2 和光源分簇集。
#[derive(bevy::prelude::Resource)]
pub struct TransparentPipeline {
    device: Device,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl TransparentPipeline {
    /// 坑：声明的颜色/深度格式必须与实际绑定的附件一致，否则报 VUID-08914。
    ///
    /// # Errors
    /// 配方对账不过，或 layout / shader module / pipeline 任一创建失败。
    pub fn new(
        ctx: &Context,
        layouts: &SetLayouts,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self> {
        let device = &ctx.device;
        TRANSPARENT_FORWARD_PIPELINE.build(device, layouts, |layout, stages| unsafe {
            Self::create_pipeline(device, color_format, depth_format, layout, stages)
        })
    }

    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        color_format: vk::Format,
        depth_format: vk::Format,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点格式与场景管线一致：位置 12B + 法线 12B + UV 8B，共 32B
        let vertex_binding = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(crate::render::VERTEX_STRIDE as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(12),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(24),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_binding)
            .vertex_attribute_descriptions(&vertex_attributes);
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // reverse-Z 用 GREATER；透明不写深度
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::GREATER)
            .min_depth_bounds(0.0)
            .max_depth_bounds(1.0);
        // 非预乘 alpha 混合：src·α + dst·(1−α)；alpha 通道直接写 src
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachment);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [color_format];
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(depth_format);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_info)
            .layout(layout)
            .push_next(&mut rendering_info);
        let pipeline = create_graphics_pipeline(device, &info)?;
        info!(
            "透明前向管线就绪: HDR {color_format:?}（SRC_ALPHA 混合）+ 深度 {depth_format:?}（重绑只测不写），常驻三集 + 光源分簇集"
        );
        Ok(Self {
            device: device.clone(),
            layout,
            pipeline,
        })
    }

    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }

    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }
}

// Safety：只存 Vulkan 句柄，跨线程只读；创建和销毁都在持有方线程
unsafe impl Send for TransparentPipeline {}
unsafe impl Sync for TransparentPipeline {}

impl Drop for TransparentPipeline {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown 已先 device_wait_idle；set 布局归各自资源销毁
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    /// 与 transparent_sort.wgsl 相同的排序网络，CPU 上跑。
    fn bitonic_sort(entries: &mut [(f32, u32)]) {
        let n = entries.len() as u32;
        let mut k = 2;
        while k <= n {
            let mut j = k / 2;
            while j > 0 {
                for gid in 0..n / 2 {
                    let i = (gid / j) * (2 * j) + (gid % j);
                    let partner = i + j;
                    let ascending = (i & k) == 0 || k == n;
                    if (ascending && entries[i as usize].0 > entries[partner as usize].0)
                        || (!ascending && entries[i as usize].0 < entries[partner as usize].0)
                    {
                        entries.swap(i as usize, partner as usize);
                    }
                }
                j /= 2;
            }
            k *= 2;
        }
    }

    #[test]
    fn bitonic_half_dispatch_sorts_entire_network_and_keeps_indices_paired() {
        let mut entries = vec![
            (3.0_f32, 3),
            (0.0, 0),
            (2.0, 2),
            (1.0, 1),
            (7.0, 7),
            (4.0, 4),
            (6.0, 6),
            (5.0, 5),
        ];
        bitonic_sort(&mut entries);
        assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(entries.iter().map(|entry| entry.1).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn bitonic_padding_moves_before_live_entries() {
        let mut entries = vec![
            (f32::NEG_INFINITY, 0),
            (f32::NEG_INFINITY, 1),
            (f32::NEG_INFINITY, 2),
            (f32::NEG_INFINITY, 3),
            (f32::NEG_INFINITY, 4),
            (f32::NEG_INFINITY, 5),
            (9.0, 90),
            (2.0, 20),
        ];
        bitonic_sort(&mut entries);
        assert_eq!(entries[6..].iter().map(|entry| entry.1).collect::<Vec<_>>(), [20, 90]);
        assert_eq!(entries[6..].iter().map(|entry| entry.0).collect::<Vec<_>>(), [2.0, 9.0]);
    }
}
