//! 帧级资源（施工④从 vulkan.rs 拆出；3.4 生长出深度附件与绘制录制）：命令池/命令缓冲
//! + 每帧"image_available + fence + 深度附件"。
//!
//! 生命周期四层里的"帧级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：MAX_FRAMES_IN_FLIGHT 组
//! 轮转复用，CPU 最多领先 GPU 这么多帧。与 swapchain 解耦的是帧槽对象，修复后各归其位：
//! - in_flight fence：GPU → CPU 重录闸门，按帧槽轮转；
//! - image_available：present engine → GPU 渲染，按帧槽轮转（复用安全前提：同槽两次
//!   acquire 隔 MAX_FRAMES_IN_FLIGHT 帧，且 fence 保证上一轮提交真的执行完了——
//!   提交失败路径一律 Tier② 退出，不留"signal 无人等"的残局）；
//! - render_finished（GPU → present engine）：**按 swapchain image 分配**，住在
//!   [`super::swapchain::Swapchain`]（D3）。同一 image 再次被 acquire 即证明其上一轮
//!   present 已被 present engine 消费完——这是帧 fence 给不了的保证（fence 只证提交
//!   完成，不证 present engine 的消费进度；依据 Khronos Vulkan Guide "Swapchain
//!   semaphore reuse"；同一队列上提交有序，下一次 signal 排在前一个 present 消费之后）。
//! - **深度附件（3.4.3）按帧槽分配**（面板待决定案）：数量 = MAX_FRAMES_IN_FLIGHT，
//!   不挂 swapchain image。两份同步证据：① 在飞的两帧各用各的槽位深度，并发写
//!   不可能撞同一张图；② 同槽深度的两次使用之间隔着 `wait_for_slot` 的 fence
//!   等待（录制前 CPU 已确认上一轮提交完成）——不把"归帧级"当无冲突证明，冲突
//!   不存在是由 fence + 槽位隔离证出来的。尺寸随 swapchain 重建（[`Self::rebuild_depth`]，
//!   extent 不变即空手而归），销毁前自带 device_wait_idle 与 swapchain 重建同纪律。
//!
//! 清屏+绘制走 Vulkan 1.3 动态渲染（feature 在 `super::context` 设备创建时已开）：
//! 无 RenderPass/Framebuffer，`loadOp=CLEAR` 就是清屏。每帧的布局纪律：
//! - 颜色：acquire 后布局不确定 → UNDEFINED→COLOR_ATTACHMENT_OPTIMAL（进场），
//!   画完 → PRESENT_SRC_KHR（出场）；
//! - 深度：每帧 UNDEFINED→DEPTH_STENCIL_ATTACHMENT_OPTIMAL（UNDEFINED 作 oldLayout
//!   = 声明"旧内容不要了"，与 loadOp CLEAR 配套；storeOp DON'T CARE——深度从不
//!   被读回，出场屏障不存在）。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::error::VulkanError;
use crate::vulkan::pipeline::{pack_push, PushData};
use crate::vulkan::Context;

/// CPU 领先 GPU 的最大帧数：第 N 帧要等第 N-2 帧的组资源空出来才能重录。
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// 深度附件格式（3.4.3 定案：D32_SFLOAT，reverse-Z 深度链的载体）。支持查询在
/// [`DepthTarget::new`]（支持与启用分开，证据落日志）；管线的深度附件格式声明与
/// 这里同一常量（pipeline.rs 只消费不另立）。
pub const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

/// 一个 draw 的绘制参数（3.4.5 DrawList 的一行）：push 数据 + 池内引脚。
/// push 字节排布契约在 [`crate::vulkan::pipeline`]（pack_push），本结构是宿主侧
/// 的取数形态——host 从快照/驻留账本/资产容器组出来，录制层只消费。
#[derive(Clone, Copy, Debug)]
pub struct DrawCall {
    pub push: PushData,
    /// `vkCmdDrawIndexed` 的 vertexOffset（字节偏移 ÷ 32，来自 MeshSlot）。
    pub vertex_base: u32,
    /// 索引池内首索引号（字节偏移 ÷ 4，来自 MeshSlot）。
    pub first_index: u32,
    pub index_count: u32,
}

/// 一次绘制帧录制的全部输入（draw_frame 组装，`record_frame` 消费）。
pub struct FrameDraw<'a> {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// set0 常驻表（bindless 双表，整帧绑一次）。
    pub set0: vk::DescriptorSet,
    /// set1 本帧槽的 UBO 集（内容 = 相机 view_proj，3.5 扩灯光）。
    pub set1: vk::DescriptorSet,
    pub vertex_buffer: Option<vk::Buffer>,
    pub index_buffer: Option<vk::Buffer>,
    /// 本帧槽的深度附件 view。
    pub depth_view: vk::ImageView,
    pub draws: &'a [DrawCall],
    /// 上传票据等待值：图形提交在 GPU 侧等 timeline ≥ 此值——"上传完成才可使用"
    /// 的执法点（0 = 无上传过，跳过等待）。该等待同时收口 transfer 写的跨队列
    /// 内存可见性（池/贴图 CONCURRENT 双族共享，无所有权屏障，3.4 定案）。
    pub wait_ticket: u64,
    pub ticket_semaphore: vk::Semaphore,
    /// 清屏颜色（线性域；SRGB view 的 ROP 编码在写出时做）。
    pub clear: [f32; 4],
}

/// 一组在飞资源。image_available 桥接 present engine → GPU 渲染，in_flight 桥接
/// GPU → CPU（重录闸门）；present 方向的 render_finished 按 image 住在 Swapchain（D3）。
/// 深度附件按帧槽各一份（3.4.3，见模块注释的同步证据）。
pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    pub image_available: vk::Semaphore,
    pub in_flight: vk::Fence,
    /// 本槽位的深度附件（D32_SFLOAT，随 swapchain 尺寸重建）。
    pub depth: DepthTarget,
}

/// 一张深度附件：VkImage（D32_SFLOAT，DEPTH_STENCIL_ATTACHMENT 用途）+ dedicated
/// memory + view（DEPTH aspect）。生命周期 = 帧槽，重建见 [`FramePool::rebuild_depth`]。
pub struct DepthTarget {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl DepthTarget {
    /// 按契约创建深度附件。格式支持先查后用（支持与启用分开，1.3 实现对
    /// D32_SFLOAT 的 depth-stencil attachment 支持是 mandatory，仍不省查询——
    /// 证据落日志）。
    ///
    /// # Errors
    /// D32_SFORMAT 的 optimal tiling feature 不足，或 Vulkan 创建/分配/绑定失败。
    pub fn new(ctx: &Context, extent: vk::Extent2D) -> Result<Self, VulkanError> {
        let format = DEPTH_FORMAT;
        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, format)
        }
        .optimal_tiling_features;
        if !features.contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT) {
            return Err(VulkanError::Init(format!(
                "深度格式 {format:?} 的 optimal tiling features {features:?} 不含 DEPTH_STENCIL_ATTACHMENT，reverse-Z 深度链无法建立"
            )));
        }
        // # Safety:create_image 参数全为合法常量 + 查过的格式/usage,无裸指针
        let image = unsafe {
            ctx.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: extent.width,
                        height: extent.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(ctx, image) };
        if result.is_err() {
            unsafe { ctx.device.destroy_image(image, None) };
        }
        result
    }

    /// 创建链后半段(image 已建):失败路径回收 memory。
    ///
    /// # Safety
    /// `image` 须刚创建成功且未销毁;错误返回后调用方不得再使用它。
    unsafe fn create_inner(ctx: &Context, image: vk::Image) -> Result<Self, VulkanError> {
        unsafe {
            let reqs = ctx.device.get_image_memory_requirements(image);
            let memory_type = ctx.memory_contract().find_type(
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::empty(),
            )?;
            let memory = ctx.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            if let Err(e) = ctx.device.bind_image_memory(image, memory, 0) {
                ctx.device.free_memory(memory, None);
                return Err(VulkanError::Vk(e));
            }
            // view:DEPTH aspect（D32_SFLOAT 无 stencil 面）、mip0 单层
            let view = ctx.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(DEPTH_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            );
            let view = match view {
                Ok(v) => v,
                Err(e) => {
                    ctx.device.free_memory(memory, None);
                    return Err(VulkanError::Vk(e));
                }
            };
            info!(
                "深度附件就绪: D32_SFLOAT，内存类型 {memory_type}（DEVICE_LOCAL），requirements 分配 {size}B",
                size = reqs.size,
            );
            Ok(Self {
                device: ctx.device.clone(),
                image,
                memory,
                view,
            })
        }
    }
}

impl Drop for DepthTarget {
    fn drop(&mut self) {
        // 契约边界:不等 GPU(重建路径的等待在 rebuild_depth,退出排空在 teardown D4)。
        // view 先于 image,memory 最后。
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

#[derive(bevy::prelude::Resource)]
pub struct FramePool {
    device: Device,
    command_pool: vk::CommandPool,
    frames: Vec<Frame>,
    /// 帧槽位游标：与 swapchain 的 image index 是两个独立循环（见模块注释）
    current: usize,
    /// 深度附件的当前尺寸（与 swapchain.extent 同步，rebuild_depth 的幂等判据）。
    depth_extent: vk::Extent2D,
}

impl FramePool {
    /// 建帧资源：命令池/命令缓冲 + 双信号量/fence + 每槽一张深度附件（尺寸取
    /// swapchain 首建 extent，重建走 [`Self::rebuild_depth`]）。
    ///
    /// # Errors
    /// 命令池/命令缓冲/信号量/fence/深度附件创建失败。
    pub fn new(ctx: &Context, swapchain_extent: vk::Extent2D) -> Result<Self, VulkanError> {
        let command_pool = unsafe {
            ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 单线程录制 + 每帧整体重录，RESET_COMMAND_BUFFER 足够
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family_index),
                None,
            )
        }?;
        let command_buffers = unsafe {
            ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
            )
        }?;
        let frames = command_buffers
            .iter()
            .map(|&command_buffer| {
                Ok(Frame {
                    command_buffer,
                    image_available: unsafe {
                        ctx.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    }?,
                    // 初值 SIGNALED：第一帧"等上一轮完成"立即通过
                    in_flight: unsafe {
                        ctx.device.create_fence(
                            &vk::FenceCreateInfo::default()
                                .flags(vk::FenceCreateFlags::SIGNALED),
                            None,
                        )
                    }?,
                    depth: DepthTarget::new(ctx, swapchain_extent)?,
                })
            })
            .collect::<Result<Vec<_>, VulkanError>>()?;
        info!(
            "帧资源就绪：{MAX_FRAMES_IN_FLIGHT} 组在飞（命令缓冲 + image_available + fence + 深度附件 {}x{}；render_finished 按 image 住在 Swapchain）",
            swapchain_extent.width, swapchain_extent.height,
        );
        Ok(Self {
            device: ctx.device.clone(),
            command_pool,
            frames,
            current: 0,
            depth_extent: swapchain_extent,
        })
    }

    /// 当前帧槽位。与 `wait_and_reset` 之间不得 `advance`。
    pub fn current(&self) -> &Frame {
        &self.frames[self.current]
    }

    /// 当前帧槽位号（0/1）：set1 每帧 UBO 按它取（`BindlessTables::frame_set/frame_ubo`）。
    #[must_use]
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 轮转到下一组帧资源（提交完成后调）。
    pub fn advance(&mut self) {
        self.current = (self.current + 1) % self.frames.len();
    }

    /// 深度附件随 swapchain 尺寸重建（幂等：extent 没变就空手而归）。调用方是
    /// host 的 resize 闸门，紧跟成功的 `Swapchain::rebuild`；销毁旧深度前自带
    /// device_wait_idle——在飞帧可能还引用着旧深度，与 swapchain 重建同纪律。
    ///
    /// # Errors
    /// 等待排空或新深度附件创建失败。
    pub fn rebuild_depth(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<(), VulkanError> {
        if self.depth_extent == extent {
            return Ok(());
        }
        unsafe { ctx.device.device_wait_idle() }?;
        for frame in &mut self.frames {
            frame.depth = DepthTarget::new(ctx, extent)?;
        }
        self.depth_extent = extent;
        info!(
            "深度附件随 swapchain 重建: {}x{} × {} 帧槽",
            extent.width,
            extent.height,
            self.frames.len(),
        );
        Ok(())
    }

    /// 帧首：等本槽位上一轮提交完成（GPU 侧 fence），重置命令缓冲。
    ///
    /// D2 定案：这里**只等不重置 fence**。fence 的重置挪到
    /// [`Self::record_frame`] 提交前一刻——若在帧首重置，其后任何提前
    /// return（acquire OUT_OF_DATE / acquire 失败 / 重建失败）都会留下一个永远
    /// 无人 signal 的 unsignaled fence，下一帧 `wait_for_fences(u64::MAX)`
    /// 永久阻塞主线程和消息泵。
    pub fn wait_for_slot(&self) -> Result<(), VulkanError> {
        let frame = &self.frames[self.current];
        unsafe {
            self.device
                .wait_for_fences(slice::from_ref(&frame.in_flight), true, u64::MAX)?;
            self.device.reset_command_buffer(
                frame.command_buffer,
                vk::CommandBufferResetFlags::empty(),
            )?;
        }
        Ok(())
    }

    /// 录制并提交一帧：进场屏障（颜色+深度）→ 待办 acquire（异族让渡）→
    /// dynamic rendering（清屏 + DrawList 逐 draw）→ 出场屏障（颜色）→
    /// queue_submit。提交等 acquire 的 image_available + 上传票据（GPU 侧），
    /// 完成时发 render_finished + fence。
    /// `render_finished` 按 acquire 的 image index 从 Swapchain 取（D3），不按帧槽轮转。
    ///
    /// # Errors
    /// 命令缓冲重置/录制/提交任一失败（调用方按 Tier② 退出，见 host::draw_frame）。
    pub fn record_frame(
        &self,
        ctx: &Context,
        render_finished: vk::Semaphore,
        image: vk::Image,
        view: vk::ImageView,
        extent: vk::Extent2D,
        draw: &FrameDraw<'_>,
    ) -> Result<(), VulkanError> {
        let frame = &self.frames[self.current];
        let device = &self.device;
        unsafe {
            device.begin_command_buffer(
                frame.command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // —— 进场 ①：颜色布局迁移（D6 定案形状）。布局迁移是一次写访问，必须
            // 排在 acquire 对 image 的读之后——提交在 COLOR_ATTACHMENT_OUTPUT 阶段等
            // image_available（等待点语义见下方提交段），屏障 srcStage 就提到这里；
            // dst_access 带上 COLOR_ATTACHMENT_WRITE，与后续 loadOp 清屏写收口。
            let to_color_attachment = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_color_attachment],
            );

            // —— 进场 ②：深度 UNDEFINED→附件布局（UNDEFINED = 旧内容不要，与
            // loadOp CLEAR 配套；storeOp DON'T CARE，故无出场屏障）。src 作用域为空
            // 的合法性：本槽深度图的上一轮使用已被 wait_for_slot 的 fence 等完
            //（3.4.3 的同步证据②），录制时不存在并发引用。
            let to_depth_attachment = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(frame.depth_image())
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::DEPTH)
                        .level_count(1)
                        .layer_count(1),
                )
                .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE);
            device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_depth_attachment],
            );

            // —— 进场 ③：无（3.4 定案）——跨族贴图不再走 release/acquire 让渡
            //（图像 CONCURRENT 双族共享，与池同款）；上传批次的图像布局收尾
            // （TRANSFER_DST→SHADER_READ_ONLY，IGNORED 族）由 uploader 图像段
            // 在 transfer 提交内完成，跨队列可见性由下方票据等待收口。
            // EXCLUSIVE 成对语义保留在 image_probe 组 C 作机制实证。

            // —— dynamic rendering：清屏 + 绘制。颜色 loadOp CLEAR（线性域清值，
            // SRGB view 的编码在 ROP 写出时做）；深度 loadOp CLEAR 清 0（reverse-Z
            // 近平面），storeOp DON'T CARE（深度从不读回）。
            let color_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: draw.clear,
                    },
                });
            let depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(frame.depth.view)
                .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::DONT_CARE)
                .clear_value(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: 0.0,
                        stencil: 0,
                    },
                });
            device.cmd_begin_rendering(
                frame.command_buffer,
                &vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    })
                    .layer_count(1)
                    .color_attachments(slice::from_ref(&color_attachment))
                    .depth_attachment(&depth_attachment),
            );

            if !draw.draws.is_empty() {
                // 面板定案：draws 非空时池 buffer 必在（upload 链先于帧循环驻留成功
                // 才会有 draw），None 是状态矛盾——panic 在录制路径按 Tier② 语义
                // 直接退进程（比静默跳过整个场景诚实）
                let Some(vertex_buffer) = draw.vertex_buffer else {
                    panic!("DrawList 非空但顶点池缺席——上传链与绘制链的状态矛盾");
                };
                let Some(index_buffer) = draw.index_buffer else {
                    panic!("DrawList 非空但索引池缺席——上传链与绘制链的状态矛盾");
                };
                device.cmd_bind_pipeline(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.pipeline,
                );
                // 常驻双集整帧绑一次：set0 = bindless 表（槽位在飞更新即生效），
                // set1 = 本帧槽 UBO（内容 = 相机 view_proj）
                let sets = [draw.set0, draw.set1];
                device.cmd_bind_descriptor_sets(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.layout,
                    0,
                    &sets,
                    &[],
                );
                // 动态 viewport/scissor：正高度直出（3.4 朝向定案，见 pipeline.rs 模块
                // 注释与施工记录 §viewport——E5 光栅级实验 + 官方示例对照截图实证；
                // 负高度翻转反而上下颠倒，与"教科书式 Vulkan 翻转"的直觉相反）
                device.cmd_set_viewport(
                    frame.command_buffer,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: extent.width as f32,
                        height: extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                device.cmd_set_scissor(
                    frame.command_buffer,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    }],
                );
                // 池整绑一次（offset 0），逐 draw 用 vertexOffset/firstIndex 定位
                device.cmd_bind_vertex_buffers(frame.command_buffer, 0, &[vertex_buffer], &[0]);
                device.cmd_bind_index_buffer(
                    frame.command_buffer,
                    index_buffer,
                    0,
                    vk::IndexType::UINT32,
                );
                for call in draw.draws {
                    let push_bytes = pack_push(&call.push);
                    device.cmd_push_constants(
                        frame.command_buffer,
                        draw.layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        &push_bytes,
                    );
                    device.cmd_draw_indexed(
                        frame.command_buffer,
                        call.index_count,
                        1,
                        call.first_index,
                        // vertexOffset 的 SPIR-V 类型是带符号的（可负偏移做实例复用
                        // 等技巧）；池引脚恒为正字节偏移÷32，值域安全
                        call.vertex_base as i32,
                        0,
                    );
                }
            }

            device.cmd_end_rendering(frame.command_buffer);

            // —— 出场：颜色画完到 present 引擎接管之间必须收口；PRESENT_SRC 是
            // present 专用布局。深度无出场（storeOp DON'T CARE + 下帧 UNDEFINED 重进）。
            let to_present_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present_src],
            );
            device.end_command_buffer(frame.command_buffer)?;

            // D2：fence 在提交前一刻才重置。此处到 queue_submit 之间没有任何提前
            // return：reset 失败 → fence 保持 signaled，下帧等待照常通过；
            // submit 失败 → 无任何提交负责 signal 这个 fence，帧循环按 Tier② 退出
            //（host.rs），不会带着残局回来等待——"等无人 signal 的 fence"死锁面关闭。
            device.reset_fences(slice::from_ref(&frame.in_flight))?;

            // 等待点语义：
            // ① COLOR_ATTACHMENT_OUTPUT 起才真正需要 image 已到手——之前的顶点处理等
            //    可以和 acquire 重叠；
            // ② ALL_COMMANDS 起等上传票据（timeline，值 = 已发出最高票据）——"上传
            //    完成才可使用"的等待发生在 GPU 执行依赖处，CPU 不先阻塞（施工计划
            //    §2；探针组 D 同款形状）。该内存依赖同时把 transfer 写对池/贴图的
            //    可见性收口（CONCURRENT 池不设所有权屏障，可见性全靠这里）。
            // 票据值 0（从未有上传提交，VUID-03242 族禁止等待 0 值）只在等待列表里
            // 留 acquire——fallback 批在 init 就提交，实际到达不了这个分支，留作
            // 形状完备。
            let (wait_semaphores, wait_dst_stage, wait_values): (
                Vec<vk::Semaphore>,
                Vec<vk::PipelineStageFlags>,
                Vec<u64>,
            ) = if draw.wait_ticket == 0 {
                (
                    vec![frame.image_available],
                    vec![vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT],
                    vec![0],
                )
            } else {
                (
                    vec![frame.image_available, draw.ticket_semaphore],
                    vec![
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                    ],
                    vec![0, draw.wait_ticket],
                )
            };
            let mut timeline_submit =
                vk::TimelineSemaphoreSubmitInfo::default().wait_semaphore_values(&wait_values);
            device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait_semaphores)
                    .wait_dst_stage_mask(&wait_dst_stage)
                    .command_buffers(slice::from_ref(&frame.command_buffer))
                    .signal_semaphores(slice::from_ref(&render_finished))
                    .push_next(&mut timeline_submit)],
                frame.in_flight,
            )?;
        }
        Ok(())
    }
}

impl Frame {
    /// 本槽位深度 image（进场屏障的目标）。
    fn depth_image(&self) -> vk::Image {
        self.depth.image
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // 同步对象逐个销毁；命令池销毁连带释放其命令缓冲；深度附件随 Frame Drop
        //（DepthTarget 自拆 view/image/memory）
        for frame in self.frames.drain(..) {
            unsafe {
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_fence(frame.in_flight, None);
            }
            drop(frame.depth);
        }
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}
