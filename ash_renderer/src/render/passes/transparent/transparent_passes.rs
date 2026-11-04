//! 透明层的三条 compute 管线（collect/sort/gather）、每帧录制输入与录制函数。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::put_u32;
use crate::common::error::{Result, VulkanError};
use crate::render::core::pipeline::compute::ComputePipeline;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::core::record::graphics::{
    bind_graphics, bind_vertex_index, full_rect, set_viewport_scissor, PipelineRef,
};
use crate::render::{Context, INDIRECT_COMMAND_SIZE};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::set_layout::{DescSet, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::passes::lighting::cluster::LIGHT_CLUSTER_SET_DESC;
use crate::render::resident::tables::instance_table::INSTANCE_SET_DESC;

use super::transparent_targets::{TRANSPARENT_CAPACITY, TRANSPARENT_SET_DESC};

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

pub(crate) static TRANSPARENT_COLLECT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_collect",
    shader: &shader_abi::TRANSPARENT_COLLECT,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC, &LIGHT_CLUSTER_SET_DESC, &TRANSPARENT_SET_DESC],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_PUSH_SIZE }),
    color_formats: &[],
    depth_format: None,
};

pub(crate) static TRANSPARENT_SORT_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_sort",
    shader: &shader_abi::TRANSPARENT_SORT,
    entries: &[c"cs_main"],
    sets: &[&TRANSPARENT_SET_DESC],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_SORT_PUSH }),
    color_formats: &[],
    depth_format: None,
};

pub(crate) static TRANSPARENT_GATHER_PIPELINE: PipelineDesc = PipelineDesc {
    label: "transparent_gather",
    shader: &shader_abi::TRANSPARENT_GATHER,
    entries: &[c"cs_main"],
    sets: &[&INSTANCE_SET_DESC, &TRANSPARENT_SET_DESC],
    push: Some(PushRange { stages: CS, size: TRANSPARENT_PUSH_SIZE }),
    color_formats: &[],
    depth_format: None,
};

/// collect/gather 的 push 大小：一个 u32 在场行数。
pub const TRANSPARENT_PUSH_SIZE: u32 = 4;

/// 一帧透明层的录制输入。光源分簇集和 cluster_build 不在这里，在 `FrameDraw`
/// 上，因为不透明合成也要用。
pub struct TransparentFrame {
    pub transparent_set: DescSet,
    /// 在场行数 > 0 才 Some。
    pub collect: Option<TransparentCollectDraw>,
    pub gather: Option<TransparentGatherDraw>,
    /// 坑：只要场景非空每帧都要排，透明行从 0 变多的那一帧也不能读未排序的数组。
    pub sort: TransparentSortPlan,
    pub pipeline: PipelineRef,
    /// 本帧槽透明命令环（t_ 前缀 = transparent，区别于主管线的命令环）：gather 把
    /// 排好序的透明行写成 IndirectCommand（20B，与 build_indirect 同布局），透明
    /// 前向一次 indirect 读。t_ 系列与 shader 内 t_index_list/t_keys 同名同源。
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

/// 透明层的三条 compute 管线。绑定的 set：collect 绑 instance_set、光源分簇集、透明集；
/// sort 只绑透明集；gather 绑 instance_set、透明集。
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
        let sort_passes = bitonic_sequence(TRANSPARENT_CAPACITY)?;
        let sort = ComputePipeline::new(ctx, &TRANSPARENT_SORT_PIPELINE, layouts)?;
        let collect = ComputePipeline::new(ctx, &TRANSPARENT_COLLECT_PIPELINE, layouts)?;
        let gather = ComputePipeline::new(ctx, &TRANSPARENT_GATHER_PIPELINE, layouts)?;
        info!(
            "透明层 compute 三件就绪: collect（[instance_set, cluster, transparent]）/ sort（[transparent]）/ gather（[instance_set, transparent]）"
        );
        Ok(Self { sort, sort_passes, collect, gather })
    }

    /// # Errors
    /// 在场行数超过 [`TRANSPARENT_CAPACITY`]。
    pub fn collect(&self, count: u32) -> Result<TransparentCollectDraw> {
        if count > TRANSPARENT_CAPACITY {
            return Err(VulkanError::Capacity(format!(
                "在场行数 {count} 超出透明行容量 {TRANSPARENT_CAPACITY}——显式报错不截断"
            )));
        }
        Ok(TransparentCollectDraw {
            pass: self.collect.handles(),
            push: count_push(count),
            groups: count.div_ceil(64),
        })
    }

    #[must_use]
    pub fn gather(&self, count: u32) -> TransparentGatherDraw {
        TransparentGatherDraw {
            pass: self.gather.handles(),
            push: count_push(count),
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

/// collect/gather 的 push：在场行数 @0。
fn count_push(count: u32) -> [u8; TRANSPARENT_PUSH_SIZE as usize] {
    let mut push = [0u8; TRANSPARENT_PUSH_SIZE as usize];
    put_u32(&mut push, 0, count);
    push
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

impl TransparentCollectDraw {
    /// 录制 collect；自带屏障让后面的 compute 读到列表、键和计数。instance/cluster 集
    /// 不在本束（不透明合成共用），录制时传入。
    pub(crate) fn record(
        &self,
        device: &Device,
        command_buffer: vk::CommandBuffer,
        instance_set: DescSet,
        cluster_set: DescSet,
        transparent_set: DescSet,
    ) {
        self.pass.record(
            device,
            command_buffer,
            &[instance_set, cluster_set, transparent_set],
            &self.push,
            self.groups,
            Handoff::ComputeRead,
        );
    }
}

impl TransparentSortPlan {
    /// 录制排序：只绑一次，逐个 (k, j) dispatch；每次之间加屏障，下一轮才能读到
    /// 上一轮的交换结果。透明集不在本束，录制时传入。
    pub(crate) fn record(&self, device: &Device, command_buffer: vk::CommandBuffer, transparent_set: DescSet) {
        self.pass.bind(device, command_buffer, &[transparent_set]);
        for pass in &self.passes {
            self.pass.dispatch(device, command_buffer, pass, self.groups, Handoff::ComputeReadWrite);
        }
    }
}

impl TransparentGatherDraw {
    /// 录制 gather；自带屏障让后面的间接绘制读到新命令。instance 集不在本束（不透明
    /// 合成共用），录制时传入。
    pub(crate) fn record(
        &self,
        device: &Device,
        command_buffer: vk::CommandBuffer,
        instance_set: DescSet,
        transparent_set: DescSet,
    ) {
        self.pass.record(
            device,
            command_buffer,
            &[instance_set, transparent_set],
            &self.push,
            self.groups,
            Handoff::IndirectCommandRead,
        );
    }
}

/// 录制透明前向：合成写 HDR → 混合读写的显式内存屏障 → HDR LOAD + 深度只测不写的
/// 一次间接绘制。两次动态渲染之间没有光栅序保证，合成写 HDR → 透明混合读写必须
/// 显式屏障；深度在本段转回附件（只测不写），⑥ 段 Hi-Z 归约再转回只读。
pub(crate) fn record_forward(
    device: &Device,
    cb: vk::CommandBuffer,
    t: &TransparentFrame,
    hdr_view: vk::ImageView,
    depth_image: vk::Image,
    depth_view: vk::ImageView,
    extent: vk::Extent2D,
    sets: &[DescSet; 4],
    pool: (vk::Buffer, vk::Buffer),
) {
    unsafe {
        let hdr_write_to_blend = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            );
        ImageBarrier::depth_read_only_to_attachment(depth_image)
            .record_with_memory(device, cb, std::slice::from_ref(&hdr_write_to_blend));
        let transparent_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(hdr_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let transparent_depth = vk::RenderingAttachmentInfo::default()
            .image_view(depth_view)
            .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::DONT_CARE);
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(full_rect(extent))
                .layer_count(1)
                .color_attachments(std::slice::from_ref(&transparent_attachment))
                .depth_attachment(&transparent_depth),
        );
        bind_graphics(device, cb, &t.pipeline, sets);
        set_viewport_scissor(device, cb, extent);
        bind_vertex_index(device, cb, pool.0, pool.1);
        // 一次 indirect 消费透明命令环（非透明行 instanceCount=0）
        device.cmd_draw_indexed_indirect(
            cb,
            t.t_commands,
            0,
            t.command_count,
            INDIRECT_COMMAND_SIZE as u32,
        );
        device.cmd_end_rendering(cb);
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
