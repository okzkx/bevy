//! shadow 资源族与 depth-only 绘制管线及其录制：四张级联深度图、shadow UBO、标记环、命令环、
//! 描述符集。shadow 域的 binding 全收在本 set，不动 instance set2：b0 UBO、b1..4 级联
//! 深度图（合成采样）、b5 交错标记环（cull 写/host 读）、b6..9 四 cascade 命令环
//! （shadow_build 写/shadow draw 读）。
//!
//! 深度图 = [`DepthTarget`] ×4（reverse-Z，清 0），固定 [`SHADOW_MAP_SIZE`]² 不随
//! swapchain resize 重建（级联覆盖面与屏幕分辨率无关）。
//!
//! UBO/标记/命令环 ×[`MAX_FRAMES_IN_FLIGHT`]，host 读写都在 `wait_for_slot` 之后。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::bytes::put_u32;
use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::core::record::graphics::{
    bind_graphics, bind_vertex_index, full_rect, set_viewport_scissor, PipelineRef,
};
use crate::render::core::device::memory::role::BufferRole;
use crate::render::passes::shadow::cascades::{SHADOW_CASCADES, SHADOW_MAP_SIZE, SHADOW_UBO_SIZE};
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

use crate::render::core::target::depth::{DepthTarget, DEPTH_FORMAT};
use crate::render::core::pipeline::graphics::{create_graphics_pipeline, PipelineHandle};
use crate::render::core::binding::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc, SetLayouts};
use crate::render::core::binding::shader_abi;
use crate::render::resident::tables::instance_table::{INDIRECT_COMMAND_SIZE, INSTANCE_SET_DESC, MARK_SIZE, OBJECT_TABLE_CAPACITY};

pub const SHADOW_BINDING_UNIFORM: u32 = 0;
const SHADOW_BINDING_MAP0: u32 = 1;
const SHADOW_BINDING_MARK0: u32 = 5;
const SHADOW_BINDING_CMD0: u32 = 6;

const _: () = assert!(SHADOW_CASCADES == 4, "SHADOW_SET_DESC 按 4 级 cascade 逐个列 binding");
const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;
const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// shadow 集：b0 均匀数据（绘制/合成/剔除都读）/ b1..b4 各级阴影图（合成采样）/
/// b5 剔除标记 / b6..b9 各级命令环（后两类只给 compute）。
pub(crate) static SHADOW_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "shadow",
    &[
        BindingDesc::new(
            SHADOW_BINDING_UNIFORM,
            DescKind::UniformBuffer,
            stages(&[vk::ShaderStageFlags::VERTEX, FS, CS]),
        ),
        BindingDesc::new(SHADOW_BINDING_MAP0, DescKind::SampledImage, FS),
        BindingDesc::new(SHADOW_BINDING_MAP0 + 1, DescKind::SampledImage, FS),
        BindingDesc::new(SHADOW_BINDING_MAP0 + 2, DescKind::SampledImage, FS),
        BindingDesc::new(SHADOW_BINDING_MAP0 + 3, DescKind::SampledImage, FS),
        BindingDesc::new(SHADOW_BINDING_MARK0, DescKind::StorageBuffer, CS),
        BindingDesc::new(SHADOW_BINDING_CMD0, DescKind::StorageBuffer, CS),
        BindingDesc::new(SHADOW_BINDING_CMD0 + 1, DescKind::StorageBuffer, CS),
        BindingDesc::new(SHADOW_BINDING_CMD0 + 2, DescKind::StorageBuffer, CS),
        BindingDesc::new(SHADOW_BINDING_CMD0 + 3, DescKind::StorageBuffer, CS),
    ],
);

/// 一帧 shadow 绘制的录制输入（draw_frame 组装，`record_frame` 消费）。
#[derive(Clone, Copy)]
pub struct ShadowWriteDraw {
    pub pipeline: PipelineRef,
    pub set2: DescSet,
    pub shadow_set: DescSet,
    pub views: [vk::ImageView; SHADOW_CASCADES],
    /// 深度图本体（布局屏障用——view 之外屏障吃 image）。
    pub images: [vk::Image; SHADOW_CASCADES],
    pub commands: [vk::Buffer; SHADOW_CASCADES],
    /// 顶点/索引池（indirect 命令引脚定位到池内——与主管线同一池契约）。
    pub vertex_buffer: vk::Buffer,
    pub index_buffer: vk::Buffer,
    /// 命令条数上界（= 在场行数，CPU 在账信息）。
    pub command_count: u32,
}

/// shadow 资源族（深度图 + UBO + 标记/命令环 + 描述符）。
#[derive(bevy::prelude::Resource)]
pub struct ShadowTargets {
    device: Device,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    /// 固定尺寸级联深度图（不随 resize 重建；生命周期 = 进程级）。
    maps: Vec<DepthTarget>,
    ubos: Vec<GpuBuffer>,
    marks: Vec<GpuBuffer>,
    commands: Vec<GpuBuffer>,
}

impl ShadowTargets {
    /// 建资源链：4×深度图 → 逐槽 UBO/标记/命令环 → set 布局 → pool + 描述符写入。
    /// 中途失败时已建部分由 Drop 回收。
    ///
    /// # Errors
    /// 深度图/Vulkan 创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let extent = vk::Extent2D { width: SHADOW_MAP_SIZE, height: SHADOW_MAP_SIZE };
        let maps = (0..SHADOW_CASCADES)
            .map(|c| {
                DepthTarget::new(ctx, extent)
                    .map_err(|e| VulkanError::Init(format!("cascade {c} 深度图创建失败: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut this = Self {
            device: ctx.device.clone(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            maps,
            ubos: Vec::new(),
            marks: Vec::new(),
            commands: Vec::new(),
        };
        this.build(ctx)?;
        info!(
            "shadow 资源就绪: {SHADOW_CASCADES} 级联 × {SHADOW_MAP_SIZE}² D32（reverse-Z clear 0）\
             × {} 帧槽（UBO {SHADOW_UBO_SIZE}B / 标记 / 命令环四族）",
            MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// 深度图建好后的后半段：buffer → set 布局 → pool → 描述符写入。
    fn build(&mut self, ctx: &Context) -> Result<()> {
        let slots = MAX_FRAMES_IN_FLIGHT;
        let sharing = ctx.sharing_families();
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(&ctx.device, ctx.memory_contract(), size, role, &sharing)
        };
        let capacity = u64::from(OBJECT_TABLE_CAPACITY);
        let mark_size = capacity * SHADOW_CASCADES as u64 * MARK_SIZE;
        let cmd_size = capacity * INDIRECT_COMMAND_SIZE;
        for _ in 0..slots {
            self.ubos.push(mk(BufferRole::FrameUniform, u64::from(SHADOW_UBO_SIZE))?);
            // 标记单份交错（[槽][cascade]）；命令环 ×cascade
            self.marks.push(mk(BufferRole::Storage, mark_size)?);
            for _ in 0..SHADOW_CASCADES {
                self.commands.push(mk(BufferRole::IndirectCommands, cmd_size)?);
            }
        }
        let device = &ctx.device;
        self.set_layout = SHADOW_SET_DESC.create(device)?;
        let pool_sizes = SHADOW_SET_DESC.pool_sizes_vec(slots as u32);
        let set_layouts = vec![self.set_layout; slots];
        unsafe {
            self.pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(slots as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            self.sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&set_layouts),
            )?;
        }
        // 深度图不随帧变，全槽写同一批 view；其余逐槽
        for slot in 0..slots {
            let mut writer = DescriptorWriter::new(&SHADOW_SET_DESC, self.sets[slot]);
            writer.buffer(SHADOW_BINDING_UNIFORM, self.ubos[slot].buffer(), 0, vk::WHOLE_SIZE);
            for (c, map) in self.maps.iter().enumerate() {
                writer.image(SHADOW_BINDING_MAP0 + c as u32, map.view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            }
            writer.buffer(SHADOW_BINDING_MARK0, self.marks[slot].buffer(), 0, vk::WHOLE_SIZE);
            for c in 0..SHADOW_CASCADES {
                writer.buffer(
                    SHADOW_BINDING_CMD0 + c as u32,
                    self.commands[slot * SHADOW_CASCADES + c].buffer(),
                    0,
                    vk::WHOLE_SIZE,
                );
            }
            writer.submit(device)?;
        }
        Ok(())
    }

    /// shadow set 布局句柄（管线/录制借源，拆除序先于本资源）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&SHADOW_SET_DESC, self.set_layout)
    }

    /// 本帧槽 shadow set。
    #[must_use]
    pub fn frame_set(&self, slot: usize) -> DescSet {
        DescSet::new(&SHADOW_SET_DESC, self.sets[slot])
    }

    /// 本帧槽 shadow UBO（host 写入口）。
    pub fn ubo(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.ubos[slot]
    }

    /// 本帧槽交错标记环（host 读写入口，布局 `[槽][cascade]` 4×u32）。
    pub fn mark_mut(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.marks[slot]
    }

    /// 本帧槽 cascade c 命令环（INDIRECT 用途，shadow draw 读）。
    #[must_use]
    pub fn command(&self, slot: usize, cascade: usize) -> vk::Buffer {
        self.commands[slot * SHADOW_CASCADES + cascade].buffer()
    }

    /// 级联深度图视图（录制附件用）。
    #[must_use]
    pub fn map_view(&self, cascade: usize) -> vk::ImageView {
        self.maps[cascade].view
    }

    /// 级联深度图本体（布局屏障用）。
    #[must_use]
    pub fn map_image(&self, cascade: usize) -> vk::Image {
        self.maps[cascade].image
    }
}

impl Drop for ShadowTargets {
    fn drop(&mut self) {
        // new 中途失败时句柄可能是 null，destroy null 是规范允许的 no-op。
        // 深度图/UBO/标记/命令环随字段 Drop，退出排空由 teardown 的 device_wait_idle 先行
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// shadow 域 depth-only 绘制管线 + layout（[set2 实例表, shadow set] + 4B push =
/// cascade 索引）。无色附件，深度 D32 reverse-Z（清 0 + GREATER，与主链同向）。
#[derive(bevy::prelude::Resource)]
pub struct ShadowPipeline {
    handle: PipelineHandle,
}

/// 4B push：cascade 索引（逐 pass 推一次）。
const SHADOW_PUSH_SIZE: u32 = 4;

pub(crate) static SHADOW_WRITE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "shadow_write",
    shader: &shader_abi::SHADOW_WRITE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&INSTANCE_SET_DESC, &SHADOW_SET_DESC],
    push: Some(PushRange { stages: vk::ShaderStageFlags::VERTEX, size: SHADOW_PUSH_SIZE }),
    color_formats: &[],
    depth_format: Some(DEPTH_FORMAT),
};

impl ShadowPipeline {
    /// 建管线链（shader module → layout → depth-only 管线；失败全有或全无）。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        SHADOW_WRITE_PIPELINE.build(ctx, layouts, |layout, stages| {
            Self::create_pipeline(&ctx.device, layout, stages)
        })
    }

    /// 管线状态装配（layout 已建；module 由 [`PipelineDesc::build`] 回收）。
    fn create_pipeline(
        device: &Device,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点输入与主管线同一份 32B 交错格式，这里只取位置
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
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::GREATER)
            .min_depth_bounds(0.0)
            .max_depth_bounds(1.0);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        // 附件格式须等于渲染时的实际附件全集（VUID-08914）：shadow 只有深度
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default().depth_attachment_format(DEPTH_FORMAT);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .dynamic_state(&dynamic_info)
            .layout(layout)
            .push_next(&mut rendering_info);
        let pipeline = create_graphics_pipeline(device, &info)?;
        info!(
            "shadow 管线就绪: depth-only dynamic rendering（D32 reverse-Z GREATER），\
             顶点输入 32B 交错 pos 位，4B push cascade 索引，[set2, shadow set]"
        );
        Ok(Self { handle: PipelineHandle::new(device, layout, pipeline, SHADOW_WRITE_PIPELINE.sets) })
    }

    /// 录制投影（帧输入束携带）。
    #[must_use]
    pub(crate) fn rec(&self) -> PipelineRef {
        self.handle.rec()
    }
}

/// 录制 shadow depth 段（G-buffer 之前）：进场屏障 → 每个级联一次动态渲染 + 一次
/// indirect draw（reverse-Z，clear 0）→ 出场屏障。进出屏障每帧都录，`shadow` 为
/// None（pass 跳过）也一样：合成 pass 的 shadow 描述符总是绑定，布局必须始终是
/// SHADER_READ_ONLY。UNDEFINED 作 oldLayout 从任何状态都合法（每帧 CLEAR 重写）。
pub(crate) fn record(
    device: &Device,
    cb: vk::CommandBuffer,
    images: &[vk::Image],
    shadow: Option<&ShadowWriteDraw>,
) {
    unsafe {
        // 进场：→ DEPTH_ATTACHMENT。深度图跨帧共享（不分帧槽）：等上一帧的片元
        // 采样读完再覆写（WAR）
        for &shadow_image in images {
            ImageBarrier::discard_to_depth_attachment(
                shadow_image,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            )
            .record(device, cb);
        }
        if let Some(shadow) = shadow {
            let map_extent = vk::Extent2D { width: SHADOW_MAP_SIZE, height: SHADOW_MAP_SIZE };
            for (c, map_view) in shadow.views.iter().enumerate() {
                let shadow_depth_attachment = vk::RenderingAttachmentInfo::default()
                    .image_view(*map_view)
                    .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(vk::ClearValue {
                        depth_stencil: vk::ClearDepthStencilValue { depth: 0.0, stencil: 0 },
                    });
                device.cmd_begin_rendering(
                    cb,
                    &vk::RenderingInfo::default()
                        .render_area(full_rect(map_extent))
                        .layer_count(1)
                        .depth_attachment(&shadow_depth_attachment),
                );
                bind_graphics(device, cb, &shadow.pipeline, &[shadow.set2, shadow.shadow_set]);
                bind_vertex_index(device, cb, shadow.vertex_buffer, shadow.index_buffer);
                set_viewport_scissor(device, cb, map_extent);
                // 4B push = 级联索引
                let mut cascade_push = [0u8; 4];
                put_u32(&mut cascade_push, 0, c as u32);
                device.cmd_push_constants(
                    cb,
                    shadow.pipeline.layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    &cascade_push,
                );
                device.cmd_draw_indexed_indirect(
                    cb,
                    shadow.commands[c],
                    0,
                    shadow.command_count,
                    INDIRECT_COMMAND_SIZE as u32,
                );
                device.cmd_end_rendering(cb);
            }
        }
        // 出场：→ SHADER_READ_ONLY 供合成采样（同样每帧都录）
        for &shadow_image in images {
            ImageBarrier::depth_attachment_to_sampled(shadow_image).record(device, cb);
        }
    }
}
