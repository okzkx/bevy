//! 8.3 shadow 资源族：级联深度图 + shadow UBO + 标记/命令环 + 描述符集 +
//! depth-only 绘制管线。shadow 域的 binding 全部收在本 set（不复用/不解冻
//! instance set2 冻结表）：b0 UBO、b1..4 级联深度图（合成采样）、b5..8 四
//! cascade 可见标记（cull 写/宿主读）、b9..12 四 cascade 命令环（shadow_build
//! 写/shadow draw 读）。
//!
//! 深度图 = [`DepthTarget`] ×4（D32_SFLOAT，DEPTH+SAMPLED 用途，reverse-Z
//! clear 0——与主深度链同向，[`crate::render::core::kit::depth_target`] 模块注释的成套纪律）；固定
//! [`SHADOW_MAP_SIZE`]² 不随 swapchain resize 重建（级联覆盖面与屏幕分辨率
//! 无关，resize 判定线由此免受影响）。
//!
//! 帧槽纪律：UBO/标记/命令环 ×[`MAX_FRAMES_IN_FLIGHT`]，宿主读写全在
//! wait_for_slot fence 证据之后（与模型环/主标记同一纪律）。

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::{BufferRole, GpuBuffer};
use crate::render::passes::shadow::cascades::{SHADOW_CASCADES, SHADOW_MAP_SIZE, SHADOW_UBO_SIZE};
use crate::render::{Context, MAX_FRAMES_IN_FLIGHT};

use crate::render::core::kit::depth_target::DepthTarget;
use crate::render::core::kit::pipeline_handle::{create_graphics_pipeline, PipelineHandle};
use crate::render::core::kit::pipeline_layout::{PipelineDesc, PushRange};
use crate::render::core::kit::set_layout::{stages, BindingDesc, DescKind, SetLayout, SetLayoutDesc, SetLayouts};
use crate::render::core::kit::shader_abi;
use crate::render::resident::instance_table::INSTANCE_SET;

pub const SHADOW_BINDING_UNIFORM: u32 = 0;
pub const SHADOW_BINDING_MAP0: u32 = 1;
pub const SHADOW_BINDING_MARK0: u32 = 5;
pub const SHADOW_BINDING_CMD0: u32 = 6;

const _: () = assert!(SHADOW_CASCADES == 4, "SHADOW_SET 按 4 级 cascade 逐个列 binding");
const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;
const FS: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// shadow 集：b0 均匀数据（绘制/合成/剔除都读）/ b1..b4 各级阴影图（合成采样）/
/// b5 剔除标记 / b6..b9 各级命令环（后两类只给 compute）。
pub(crate) static SHADOW_SET: SetLayoutDesc = SetLayoutDesc::new(
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
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub set2: vk::DescriptorSet,
    pub shadow_set: vk::DescriptorSet,
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
    /// 建资源链：4×深度图 → 逐槽 UBO/标记/命令环 → set 布局 → pool + 描述符
    /// 写入。半建失败逐层回收（全有或全无）。
    ///
    /// # Errors
    /// 深度图/Vulkan 创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let extent = vk::Extent2D { width: SHADOW_MAP_SIZE, height: SHADOW_MAP_SIZE };
        let mut maps = Vec::with_capacity(SHADOW_CASCADES);
        let result: Result<()> = (|| {
            for c in 0..SHADOW_CASCADES {
                maps.push(DepthTarget::new(ctx, extent).map_err(|e| {
                    VulkanError::Init(format!("cascade {c} 深度图创建失败: {e}"))
                })?);
            }
            Ok(())
        })();
        result?;
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
        let built = unsafe { this.build(ctx) };
        if let Err(e) = built {
            this.destroy_partials();
            return Err(e);
        }
        info!(
            "shadow 资源就绪: {SHADOW_CASCADES} 级联 × {SHADOW_MAP_SIZE}² D32（reverse-Z clear 0）\
             × {} 帧槽（UBO {SHADOW_UBO_SIZE}B / 标记 / 命令环四族）",
            MAX_FRAMES_IN_FLIGHT,
        );
        Ok(this)
    }

    /// # Safety
    /// 深度图已建成功；失败路径由调用方回收已建半成品。
    unsafe fn build(&mut self, ctx: &Context) -> Result<()> {
        let slots = MAX_FRAMES_IN_FLIGHT;
        let sharing = ctx.sharing_families();
        let mk = |role: BufferRole, size: u64| -> Result<GpuBuffer> {
            GpuBuffer::create_with_families(&ctx.device, ctx.memory_contract(), size, role, &sharing)
        };
        let mark_size =
            u64::from(crate::render::resident::instance_table::OBJECT_TABLE_CAPACITY) * 4 * 4;
        let cmd_size = u64::from(crate::render::resident::instance_table::OBJECT_TABLE_CAPACITY) * 20;
        for _ in 0..slots {
            self.ubos.push(mk(BufferRole::FrameUniform, u64::from(SHADOW_UBO_SIZE))?);
            // 标记单份交错（[槽][cascade] 4×u32/槽）；命令环 ×cascade——
            // mark()/command() 以 slot 直址
            self.marks.push(mk(BufferRole::Storage, mark_size)?);
            for _ in 0..SHADOW_CASCADES {
                self.commands.push(mk(BufferRole::IndirectCommands, cmd_size)?);
            }
        }
        unsafe {
            let device = &ctx.device;
            self.set_layout = SHADOW_SET.create(device).inspect_err(|_| self.destroy_buffers())?;
            self.pool = device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(slots as u32)
                        .pool_sizes(&[
                            vk::DescriptorPoolSize::default()
                                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                                .descriptor_count(slots as u32),
                            vk::DescriptorPoolSize::default()
                                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                                .descriptor_count((4 * slots) as u32),
                            vk::DescriptorPoolSize::default()
                                .ty(vk::DescriptorType::STORAGE_BUFFER)
                                .descriptor_count(((1 + SHADOW_CASCADES) * slots) as u32),
                        ]),
                    None,
                )
                .inspect_err(|_| {
                    device.destroy_descriptor_set_layout(self.set_layout, None);
                    self.set_layout = vk::DescriptorSetLayout::null();
                })?;
            self.sets = device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.pool)
                        .set_layouts(
                            &std::iter::repeat_n(self.set_layout, slots).collect::<Vec<_>>(),
                        ),
                )
                .inspect_err(|_| {
                    device.destroy_descriptor_pool(self.pool, None);
                    self.pool = vk::DescriptorPool::null();
                })?;
            // 描述符写入：b0 UBO（逐槽）、b1..4 深度图 sampled view（全槽同源
            // ——固定尺寸图不随帧变）、b5 标记 / b6..9 命令环（逐槽）。
            // 两段式：先收全部 info（记账索引），再统一造 writes——避免
            // "push 下一条时上一条的 last() 借用还活着"的借用交叠。
            let mut image_infos: Vec<vk::DescriptorImageInfo> = Vec::new();
            let mut buffer_infos: Vec<vk::DescriptorBufferInfo> = Vec::new();
            // (set 索引, binding, 类型, info 在其向量里的下标)
            let mut plan: Vec<(usize, u32, vk::DescriptorType, usize, bool)> = Vec::new();
            for slot in 0..slots {
                buffer_infos.push(
                    vk::DescriptorBufferInfo::default()
                        .buffer(self.ubos[slot].buffer())
                        .offset(0)
                        .range(vk::WHOLE_SIZE),
                );
                plan.push((slot, SHADOW_BINDING_UNIFORM, vk::DescriptorType::UNIFORM_BUFFER,
                    buffer_infos.len() - 1, true));
                for (c, map) in self.maps.iter().enumerate() {
                    image_infos.push(
                        vk::DescriptorImageInfo::default()
                            .image_view(map.view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                    );
                    plan.push((slot, SHADOW_BINDING_MAP0 + c as u32, vk::DescriptorType::SAMPLED_IMAGE,
                        image_infos.len() - 1, false));
                }
                // b5 = 单份交错标记环（[槽][cascade]）
                buffer_infos.push(
                    vk::DescriptorBufferInfo::default()
                        .buffer(self.marks[slot].buffer())
                        .offset(0)
                        .range(vk::WHOLE_SIZE),
                );
                plan.push((slot, SHADOW_BINDING_MARK0, vk::DescriptorType::STORAGE_BUFFER,
                    buffer_infos.len() - 1, true));
                for c in 0..SHADOW_CASCADES {
                    buffer_infos.push(
                        vk::DescriptorBufferInfo::default()
                            .buffer(self.commands[slot * SHADOW_CASCADES + c].buffer())
                            .offset(0)
                            .range(vk::WHOLE_SIZE),
                    );
                    plan.push((slot, SHADOW_BINDING_CMD0 + c as u32, vk::DescriptorType::STORAGE_BUFFER,
                        buffer_infos.len() - 1, true));
                }
            }
            let writes: Vec<vk::WriteDescriptorSet> = plan
                .iter()
                .map(|&(slot, binding, ty, idx, is_buffer)| {
                    let set = self.sets[slot];
                    if is_buffer {
                        vk::WriteDescriptorSet::default()
                            .dst_set(set)
                            .dst_binding(binding)
                            .descriptor_type(ty)
                            .buffer_info(&buffer_infos[idx..idx + 1])
                    } else {
                        vk::WriteDescriptorSet::default()
                            .dst_set(set)
                            .dst_binding(binding)
                            .descriptor_type(ty)
                            .image_info(&image_infos[idx..idx + 1])
                    }
                })
                .collect();
            device.update_descriptor_sets(&writes, &[]);
        }
        Ok(())
    }

    fn destroy_buffers(&mut self) {
        self.ubos.clear();
        self.marks.clear();
        self.commands.clear();
    }

    fn destroy_partials(&mut self) {
        self.destroy_buffers();
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
                self.pool = vk::DescriptorPool::null();
            }
            if self.set_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout, None);
                self.set_layout = vk::DescriptorSetLayout::null();
            }
        }
        self.maps.clear();
    }

    /// shadow set 布局句柄（管线/录制借源，拆除序先于本资源）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&SHADOW_SET, self.set_layout)
    }

    /// 本帧槽 shadow set。
    #[must_use]
    pub fn frame_set(&self, slot: usize) -> vk::DescriptorSet {
        self.sets[slot]
    }

    /// 本帧槽 shadow UBO（宿主写入口，fence 纪律内）。
    pub fn ubo(&mut self, slot: usize) -> &mut GpuBuffer {
        &mut self.ubos[slot]
    }

    /// 本帧槽交错标记环（宿主读数口，布局 [槽][cascade] 4×u32）。
    #[must_use]
    pub fn mark(&self, slot: usize) -> &GpuBuffer {
        &self.marks[slot]
    }

    /// 本帧槽交错标记环（宿主全可见填充写入口，fence 纪律内）。
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
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set_layout, None);
            }
        }
        // 深度图/UBO/标记/命令环随字段 Drop——退出排空由 teardown 的
        // device_wait_idle 先行覆盖（与帧级资源同纪律）
    }
}

/// shadow 域 depth-only 绘制管线 + layout（[set2 实例表, shadow set] + 4B push
/// = cascade 索引）。dynamic rendering 无色附件，深度 D32 reverse-Z（清 0 +
/// GREATER 与主链成套）。
#[derive(bevy::prelude::Resource)]
pub struct ShadowPipeline {
    handle: PipelineHandle,
}

/// 4B push：cascade 索引（逐 pass 一推——帧级全局量的本职，cull 112B 同款理由）。
pub const SHADOW_PUSH_SIZE: u32 = 4;

pub(crate) static SHADOW_WRITE_PIPELINE: PipelineDesc = PipelineDesc {
    label: "shadow_write",
    shader: &shader_abi::SHADOW_WRITE,
    entries: &[c"vs_main", c"fs_main"],
    sets: &[&INSTANCE_SET, &SHADOW_SET],
    push: Some(PushRange { stages: vk::ShaderStageFlags::VERTEX, size: SHADOW_PUSH_SIZE }),
};

impl ShadowPipeline {
    /// 建管线链（shader module → layout → depth-only 管线；失败全有或全无）。
    ///
    /// # Errors
    /// 配方对账不过，或 shader module / layout / pipeline 任一创建失败。
    pub fn new(ctx: &Context, layouts: &SetLayouts) -> Result<Self> {
        let device = &ctx.device;
        SHADOW_WRITE_PIPELINE.build(device, layouts, |layout, stages| unsafe {
            Self::create_pipeline(device, layout, stages)
        })
    }

    /// # Safety
    /// `layout` 与 `stages` 里的 module 须为合法句柄。
    unsafe fn create_pipeline(
        device: &Device,
        layout: vk::PipelineLayout,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
    ) -> Result<Self> {
        // 顶点输入与主管线同一份 32B 交错契约（mesh_convert 同源）
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
        // dynamic rendering：无色附件、深度 D32（VUID-08914 声明面 = 实例
        // 附件全集——shadow 实例只有深度）
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .depth_attachment_format(crate::render::core::kit::depth_target::DEPTH_FORMAT);
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
        Ok(Self { handle: PipelineHandle::new(device, layout, pipeline) })
    }

    /// 管线句柄。
    #[must_use]
    pub fn pipeline(&self) -> vk::Pipeline {
        self.handle.pipeline()
    }

    /// layout 句柄。
    #[must_use]
    pub fn layout(&self) -> vk::PipelineLayout {
        self.handle.layout()
    }
}
