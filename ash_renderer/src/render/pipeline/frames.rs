//! 帧级资源：命令池/命令缓冲 + 每帧"image_available + fence"。
//!
//! 生命周期四层里的"帧级"（VulkanContext字段释义：从Entry到Swapchain.md §12）：MAX_FRAMES_IN_FLIGHT 组
//! 轮转复用，CPU 最多领先 GPU 这么多帧。与 swapchain 解耦的是帧槽对象，各归其位：
//! - in_flight fence：GPU → CPU 重录闸门，按帧槽轮转；
//! - image_available：present engine → GPU 渲染，按帧槽轮转（复用安全前提：同槽两次
//!   acquire 隔 MAX_FRAMES_IN_FLIGHT 帧，且 fence 保证上一轮提交真的执行完了——
//!   提交失败路径一律 Tier② 退出，不留"signal 无人等"的残局）；
//! - render_finished（GPU → present engine）：**按 swapchain image 分配**，住在
//!   [`super::swapchain::Swapchain`]。同一 image 再次被 acquire 即证明其上一轮
//!   present 已被 present engine 消费完——这是帧 fence 给不了的保证（fence 只证提交
//!   完成，不证 present engine 的消费进度；依据 Khronos Vulkan Guide "Swapchain
//!   semaphore reuse"；同一队列上提交有序，下一次 signal 排在前一个 present 消费之后）。
//! - **深度附件按帧槽分配**：数量 = MAX_FRAMES_IN_FLIGHT，本体在 [`super::depth`]//!   （3.6 P1 摊薄兑现位）。两份同步证据：① 在飞的两帧各用各的槽位深度，并发写
//!   不可能撞同一张图；② 同槽深度的两次使用之间隔着 `wait_for_slot` 的 fence
//!   等待（录制前 CPU 已确认上一轮提交完成）——不把"归帧级"当无冲突证明，冲突
//!   不存在是由 fence + 槽位隔离证出来的。尺寸随 swapchain 重建（[`Self::rebuild_depth`]，
//!   extent 不变即空手而归），销毁前自带 device_wait_idle 与 swapchain 重建同纪律。
//!
//! 清屏+绘制走 Vulkan 1.3 动态渲染（feature 在 `crate::render::context` 设备创建时已开）：
//! 无 RenderPass/Framebuffer，`loadOp=CLEAR` 就是清屏。每帧的布局纪律：
//! - 颜色：acquire 后布局不确定 → UNDEFINED→COLOR_ATTACHMENT_OPTIMAL（进场），
//!   画完 → PRESENT_SRC_KHR（出场）；
//! - 深度：每帧 UNDEFINED→DEPTH_STENCIL_ATTACHMENT_OPTIMAL（UNDEFINED 作 oldLayout
//!   = 声明"旧内容不要了"，与 loadOp CLEAR 配套）。**storeOp STORE（6.1.2 起
//!   改 DON'T CARE→STORE）**：深度在本帧渲染后由 Hi-Z 归约采样（hiz 模块），
//!   采完即弃——下帧 UNDEFINED 进场照常丢弃，深度内容不跨帧持久，金字塔才是
//!   持久层；出场屏障仍不存在（消费发生在同一条命令缓冲内）。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::gpu::cull::CullDraw;
use crate::render::gpu::hiz::HizPyramid;
use crate::render::gpu::indirect::IndirectDraw;
use crate::render::gpu::lod::LodDraw;
use crate::render::gpu::occlusion::OcclusionDraw;
use crate::render::GBufferTargets;
use crate::render::pipeline::cluster::TransparentFrame;
use crate::render::pipeline::overlay_pipeline::{pack_ui_push, UiPaint};
use crate::render::perf::GpuTimer;
use crate::render::Context;
use super::depth::DepthTarget;


/// CPU 领先 GPU 的最大帧数：第 N 帧要等第 N-2 帧的组资源空出来才能重录。
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// indirect 命令字节步长（`VkDrawIndexedIndirectCommand` = 20B，instance_table
/// 的 `INDIRECT_COMMAND_SIZE` 同源——draw 端 stride 与命令环步长钉同值）。
pub const INDIRECT_COMMAND_STRIDE: u32 = 20;

/// 一次绘制帧录制的全部输入（draw_frame 组装，`record_frame` 消费）。
pub struct FrameDraw<'a> {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// set0 常驻表（bindless 双表，整帧绑一次）。
    pub set0: vk::DescriptorSet,
    /// set1 本帧槽的 UBO 集（内容 = 相机 view_proj，后续扩灯光）。
    pub set1: vk::DescriptorSet,
    /// set2 本帧槽的表集（对象表 + 本帧模型/材质环，instance_table 契约）。
    pub set2: vk::DescriptorSet,
    pub vertex_buffer: Option<vk::Buffer>,
    pub index_buffer: Option<vk::Buffer>,
    /// 在场行数（= GPU 侧命令条数；0 = 空场/全暂缓，场景绘制整段跳过）。
    /// 逐对象参数在 GPU 常驻表——录制层零逐对象参数（push 与逐 draw 循环
    /// 已随 5.3 退役），判定线 1 的代码面。
    pub indirect_count: u32,
    /// 本帧槽命令环 buffer（build_indirect 每帧重写，`count > 0` 时消费）。
    pub indirect_buffer: vk::Buffer,
    /// G-buffer 附件 + HDR 中间目标（8.1：多 pass 帧编排的附件面，采样集
    /// 也在其上——合成 pass 与 present blit 消费）。
    pub gbuffer: &'a GBufferTargets,
    /// 合成管线（G-buffer 光照累积 → HDR 中间目标；全屏，无深度/混合）。
    pub composite_pipeline: vk::Pipeline,
    pub composite_layout: vk::PipelineLayout,
    /// cluster 集（8.2.3：合成光照累积的取数面——b0 网格参数 / b1 光源常驻表
    /// / b2 逐 cluster 列表；与透明层同一 layout 对象）。
    pub cluster_set: vk::DescriptorSet,
    /// present blit 管线（HDR 中间目标 → swapchain；全屏，无深度/混合）。
    pub blit_pipeline: vk::Pipeline,
    pub blit_layout: vk::PipelineLayout,
    /// overlay 管线（UI 半边的绘制入口；深度格式与实例对齐声明，读/写全关）。
    pub overlay_pipeline: vk::Pipeline,
    /// overlay 自有 pipeline layout（只借 set0，无 set1）——换管线后描述符绑定
    /// 不延续，set0 须按这个 layout 对象重绑。
    pub overlay_layout: vk::PipelineLayout,
    /// 本帧 UI 绘制产物（无 UI 帧为 None：首 pass 前空帧/超容丢弃/图集缺席）。
    pub ui: Option<&'a UiPaint>,
    /// 本帧槽 UI 顶点/索引 buffer（`ui` 为 Some 时必有当帧写入）。
    pub ui_buffers: (vk::Buffer, vk::Buffer),
    /// 视锥剔除 dispatch（5.2；空场或开关关闭为 None——关闭时 host 已把本帧
    /// 槽标记填全可见，诊断读数仍有语义）。录制在渲染进场之前（compute 写 →
    /// 屏障 → 图形读的依赖形状，消费归 5.3）。
    pub cull: Option<CullDraw>,
    /// indirect 命令组装 dispatch（5.3；在场行数为 0 为 None——空场无命令可
    /// 组装，场景绘制整段跳过）。录制在 cull dispatch 之后（标记写 → 组装读
    /// 的依赖由 cull::record 的预置屏障收口），命令 → 绘制的依赖由组装自带
    /// 屏障收口（indirect::record）。
    pub indirect: Option<IndirectDraw>,
    /// Hi-Z 遮挡测试 dispatch（6.1；fallback/关闭/空场为 None——b3 保持视锥
    /// 纯结果，b6 host 填全可见）。录制在视锥剔除之后、indirect 组装之前
    ///（视锥标记 → 遮挡合并 → 命令组装的依赖链，occlusion::record 自带屏障）。
    pub occlusion: Option<OcclusionDraw>,
    /// GPU LOD 选档 dispatch（6.2；关闭/空场为 None——档位环 host 填 0 =
    /// 最细档，命令引脚回到对象记录基准）。录制在遮挡测试之后、indirect
    /// 组装之前（档位写 → 组装读的依赖由 lod::record 自带屏障收口）。
    pub lod: Option<LodDraw>,
    /// 透明 Clustered Forward 层（8.1.3）：cluster 构建/收集/组装三 dispatch
    ///（compute 区间录制）+ 透明绘制输入（合成 pass 之后、Hi-Z 之前）。
    /// `transparent = None` = 空场（无在场行，三 dispatch 与绘制整段跳过）。
    pub transparent: Option<TransparentFrame>,
    /// Hi-Z 金字塔归约链（6.1.2）：渲染后逐级重写本帧槽（Some = 恒 Some，
    /// 重建窗口期也录——warmup 由 hiz 内部走账）。深度屏障（附件 → 只读）
    /// 与进场/级间屏障在 hiz::record_reduction 内收口。
    pub hiz: Option<&'a HizPyramid>,
    /// 上传票据等待值：图形提交在 GPU 侧等 timeline ≥ 此值——"上传完成才可使用"
    /// 的执法点（0 = 无上传过，跳过等待）。该等待同时收口 transfer 写的跨队列
    /// 内存可见性（池/贴图 CONCURRENT 双族共享，无所有权屏障，定案）。
    pub wait_ticket: u64,
    pub ticket_semaphore: vk::Semaphore,
    /// 清屏颜色（线性域；SRGB view 的 ROP 编码在写出时做）。
    pub clear: [f32; 4],
    /// GPU 帧计时（5.5，8.1 起七位）：Some = 帧首 reset+T0、段界 T1..T6
    /// 采样（compute/G-buffer/合成/Hi-Z/blit/全帧）；None = 计时器停用
    ///（队列族无 timestamp）。
    pub timer: Option<&'a GpuTimer>,
}

/// 一组在飞资源。image_available 桥接 present engine → GPU 渲染，in_flight 桥接
/// GPU → CPU（重录闸门）；present 方向的 render_finished 按 image 住在 Swapchain。
/// 深度附件按帧槽各一份（见模块注释的同步证据）。
pub struct Frame {
    pub command_buffer: vk::CommandBuffer,
    pub image_available: vk::Semaphore,
    pub in_flight: vk::Fence,
    /// 本槽位的深度附件（D32_SFLOAT，随 swapchain 尺寸重建）。
    pub depth: DepthTarget,
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
    pub fn new(ctx: &Context, swapchain_extent: vk::Extent2D) -> Result<Self> {
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
            .collect::<Result<Vec<_>>>()?;
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

    /// 当前帧槽位。与 `wait_for_slot`（等待+重置入口）之间不得 `advance`。
    pub fn current(&self) -> &Frame {
        &self.frames[self.current]
    }

    /// 当前帧槽位号（0/1）：set1 每帧 UBO 按它取（`BindlessTables::frame_set/frame_ubo`）。
    #[must_use]
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 第 `slot` 帧槽的深度 image 句柄（Hi-Z 归约 level 0 源视图的挂载目标，
    /// 6.1.2——HizPyramid 建/重建时取）。
    #[must_use]
    pub fn depth_image(&self, slot: usize) -> vk::Image {
        self.frames[slot].depth.image
    }

    /// 第 `slot` 帧槽的深度视图句柄（G-buffer 采样集 b3 的来源，8.1）。
    #[must_use]
    pub fn depth_view(&self, slot: usize) -> vk::ImageView {
        self.frames[slot].depth.view
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
    pub fn rebuild_depth(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
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

    /// 等待全部帧槽的在飞提交完成（池迁移的"图形最后使用"等待）。fence
    /// 只证提交完成：全部槽位 fence 皆 signaled ⇒ 没有任何命令缓冲还在 GPU 上
    /// 执行 ⇒ 在飞 draw 对旧池的读取已全部收口。已完成的槽立即通过，正常路径
    /// 至多等 MAX_FRAMES_IN_FLIGHT-1 个在飞尾帧；等待不重置 fence，与
    /// `wait_for_slot` 的"只等不重置"纪律一致。
    ///
    /// # Errors
    /// fence 等待失败（设备丢失等，Tier② 语义由调用方分流）。
    pub fn wait_all_inflight(&self) -> Result<()> {
        let fences: Vec<vk::Fence> = self.frames.iter().map(|f| f.in_flight).collect();
        unsafe { self.device.wait_for_fences(&fences, true, u64::MAX)? };
        Ok(())
    }

    /// 帧首：等本槽位上一轮提交完成（GPU 侧 fence），重置命令缓冲。
    ///
    /// 这里**只等不重置 fence**。fence 的重置挪到
    /// [`Self::record_frame`] 提交前一刻——若在帧首重置，其后任何提前
    /// return（acquire OUT_OF_DATE / acquire 失败 / 重建失败）都会留下一个永远
    /// 无人 signal 的 unsignaled fence，下一帧 `wait_for_fences(u64::MAX)`
    /// 永久阻塞主线程和消息泵。
    pub fn wait_for_slot(&self) -> Result<()> {
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
    /// 剔除 + indirect 命令组装 dispatch（compute，渲染进场之前）→
    /// dynamic rendering（清屏 + 一次 indirect 提交 + overlay UI 接画，同实例）→
    /// 出场屏障（颜色）→ queue_submit。提交等 acquire 的 image_available + 上传
    /// 票据（GPU 侧），完成时发 render_finished + fence。
    /// `render_finished` 按 acquire 的 image index 从 Swapchain 取，不按帧槽轮转。
    ///
    /// # Errors
    /// 命令缓冲重置/录制/提交任一失败（调用方按 Tier② 退出，见
    /// `crate::driver::host::draw_frame`）。
    pub fn record_frame(
        &self,
        ctx: &Context,
        render_finished: vk::Semaphore,
        image: vk::Image,
        view: vk::ImageView,
        extent: vk::Extent2D,
        draw: &FrameDraw<'_>,
    ) -> Result<()> {
        let frame = &self.frames[self.current];
        let device = &self.device;
        let slot = self.current;
        unsafe {
            device.begin_command_buffer(
                frame.command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // —— 帧计时（5.5）：query pool 重置 + T0（进场后原点位）。
            if let Some(timer) = draw.timer {
                timer.reset_and_start(frame.command_buffer, slot);
            }

            // —— 进场 ①：颜色布局迁移。布局迁移是一次写访问，必须
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
            //（模块注释的同步证据②），录制时不存在并发引用。
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

            // —— 进场 ③：G-buffer 附件（albedo/normal）UNDEFINED→附件布局。
            // 每帧 loadOp CLEAR 覆写全部像素，本槽上一轮的使用已被 fence 等完
            //（同深度进场②的证据），无需旧内容语义。
            for gbuffer_image in [
                draw.gbuffer.albedo_image(slot),
                draw.gbuffer.normal_image(slot),
                // 8.2.1 matparam（材质标量 r=metallic/g=AO/b=roughness）随
                // G-buffer 三附件同批进场
                draw.gbuffer.matparam_image(slot),
                // HDR 中间目标同样首用 UNDEFINED 进场（下帧 UNDEFINED 进场丢弃
                // 上一轮 SHADER_READ_ONLY 内容——loadOp CLEAR 覆写全像素）
                draw.gbuffer.hdr_image(slot),
            ] {
                let to_gbuffer_attachment = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(gbuffer_image)
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
                    slice::from_ref(&to_gbuffer_attachment),
                );
            }

            // —— 进场 ③：无——跨族贴图不走 release/acquire 让渡
            //（图像 CONCURRENT 双族共享，与池同款）；上传批次的图像布局收尾
            // （TRANSFER_DST→SHADER_READ_ONLY，IGNORED 族）由 uploader 图像段
            // 在 transfer 提交内完成，跨队列可见性由下方票据等待收口。
            // EXCLUSIVE 成对语义保留在 image_probe 组 C 作机制实证。

            // —— 透明键 -inf 预填充（8.1.4）：bitonic 全容量升序排把真实条目
            // 推到数组尾部（gather 反向取）——padding 键必须是 -inf。fill 在
            // 所有 compute dispatch 之前（TRANSFER 写 → compute 读屏障收口）。
            if let Some(t) = draw.transparent.as_ref() {
                device.cmd_fill_buffer(
                    frame.command_buffer,
                    t.t_keys,
                    0,
                    vk::WHOLE_SIZE,
                    0xFF80_0000, // f32::NEG_INFINITY 位型
                );
                let fill_barrier = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
                device.cmd_pipeline_barrier(
                    frame.command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    slice::from_ref(&fill_barrier),
                    &[],
                    &[],
                );
            }

            // —— 视锥剔除 dispatch（5.2，渲染进场之前）：compute 读 set2 三表
            // + push 平面，写标记环本帧槽；随后的屏障是 compute 写→图形读的
            // 依赖形状（消费归 5.3，判定线 3）。空场/关闭 = None（host 已把
            // 本帧槽标记填全可见，诊断语义不断）。
            if let Some(cull) = &draw.cull {
                crate::render::gpu::cull::record(device, frame.command_buffer, draw.set2, cull);
            }

            // —— Hi-Z 遮挡测试 dispatch（6.1，渲染进场之前、视锥之后）：读
            // set2 三表 + 视锥标记 + 本帧槽金字塔（两帧前归约输出，fence 纪律
            // 内），合并写回标记并落遮挡独立标记。fallback/关闭 = None。
            if let Some(occlusion) = &draw.occlusion {
                crate::render::gpu::occlusion::record(device, frame.command_buffer, draw.set2, occlusion);
            }

            // —— GPU LOD 选档 dispatch（6.2，渲染进场之前、遮挡之后）：读
            // set2 三表（对象表/模型环）做 AABB 投屏，写档位环 b8（迟滞状态
            // 读旧写新，2 帧滞后由迟滞带吸收）。关闭/空场 = None（host 已把
            // 本帧槽档位环填 0 = 最细档，命令引脚回到 5.3 基准）。
            if let Some(lod) = &draw.lod {
                crate::render::gpu::lod::record(device, frame.command_buffer, draw.set2, lod);
            }

            // —— indirect 命令组装 dispatch（5.3，渲染进场之前）：compute 读
            // 在场环（host 每帧写）+ 标记（cull 写或 host 全可见填充）+ 对象表，
            // 逐在场行组装 VkDrawIndexedIndirectCommand 进命令环本帧槽；随后
            // 自带屏障是命令写 → DRAW_INDIRECT 读的依赖形状。空场 = None。
            if let Some(indirect) = &draw.indirect {
                crate::render::gpu::indirect::record(device, frame.command_buffer, draw.set2, indirect);
            }

            // —— 透明层三 dispatch（8.1.3，渲染进场之前）：cluster 光源列表
            // 构建 → 透明行收集（alpha<1 紧凑 + 深度键）→ 命令组装。空场 =
            // transparent None（三 dispatch 与透明绘制整段跳过）。
            if let Some(t) = draw.transparent.as_ref() {
                if let Some(build) = &t.cluster_build {
                    crate::render::pipeline::cluster::record_cluster_build(
                        device,
                        frame.command_buffer,
                        t.cluster_set,
                        build,
                    );
                }
                if let Some(collect) = &t.collect {
                    crate::render::pipeline::cluster::record_transparent_collect(
                        device,
                        frame.command_buffer,
                        draw.set2,
                        t.cluster_set,
                        collect,
                    );
                }
                // bitonic 排序链（8.1.4，collect 与 gather 之间；prev 透明
                // 计数 0 的让路帧 None——未排序尾部反向读取仍覆盖全部真实行）
                if let Some(sort) = &t.sort {
                    crate::render::pipeline::cluster::record_transparent_sort(
                        device,
                        frame.command_buffer,
                        t.cluster_set,
                        sort,
                    );
                }
                if let Some(gather) = &t.gather {
                    crate::render::pipeline::cluster::record_transparent_gather(
                        device,
                        frame.command_buffer,
                        draw.set2,
                        t.cluster_set,
                        gather,
                    );
                }
            }

            // —— 帧计时 T1：compute 区间（cull+组装）结束点位。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 1);
            }

            // —— G-buffer pass（8.1 不透明 deferred 写入，多 color 附件首跑）：
            // albedo/normal 双 color + 深度，loadOp 全 CLEAR（附件清 0——背景
            // 像素内容无语义，合成端以 depth==0 判背景）；深度 loadOp CLEAR 清 0
            //（reverse-Z 近平面），storeOp STORE（合成采样 + Hi-Z 归约消费）。
            let gbuffer_attachments = [
                vk::RenderingAttachmentInfo::default()
                    .image_view(draw.gbuffer.albedo_view(slot))
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 0.0],
                        },
                    }),
                vk::RenderingAttachmentInfo::default()
                    .image_view(draw.gbuffer.normal_view(slot))
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 0.0],
                        },
                    }),
                // 8.2.1 matparam（材质标量）：附件计数与主管线声明面一致
                //（VUID-06179——管线 colorAttachmentCount = 实例 colorAttachmentCount）
                vk::RenderingAttachmentInfo::default()
                    .image_view(draw.gbuffer.matparam_view(slot))
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 1.0],
                        },
                    }),
            ];
            let depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(frame.depth.view)
                .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                // 6.1.2 起 STORE：本帧深度渲染后由合成（世界位置）与 Hi-Z 归约
                // 采样（同一条命令缓冲内）——采完即弃，下帧 UNDEFINED 进场照常丢弃
                .store_op(vk::AttachmentStoreOp::STORE)
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
                    .color_attachments(&gbuffer_attachments)
                    .depth_attachment(&depth_attachment),
            );

            // —— 场景绘制（5.3 indirect 闭环）：一次 cmd_draw_indexed_indirect
            // 消费命令环——绘制调用数不随在场对象数增长（判定线 1 的代码面）。
            // 逐 draw 循环 + 逐对象 push 4B 已退役（firstInstance 携带实例槽号，
            // VS 按 @builtin(instance_index) 读 set2）。UI 不在本 pass（挪到
            // present blit 实例）。
            if draw.indirect_count > 0 {
                // 在场行非空时池 buffer 必在（upload 链先于帧循环驻留成功才会有
                // 在场行），None 是状态矛盾——返回 Err 经统一通道按 Tier②
                // 优雅退出（比静默跳过整个场景诚实）
                let vertex_buffer = draw.vertex_buffer.ok_or_else(|| {
                    VulkanError::Upload("在场行非空但顶点池缺席——上传链与绘制链的状态矛盾".into())
                })?;
                let index_buffer = draw.index_buffer.ok_or_else(|| {
                    VulkanError::Upload("在场行非空但索引池缺席——上传链与绘制链的状态矛盾".into())
                })?;
                device.cmd_bind_pipeline(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.pipeline,
                );
                // 常驻三集整帧绑一次：set0 = bindless 表（槽位在飞更新即生效），
                // set1 = 本帧槽 UBO（内容 = 相机 view_proj），set2 = GPU 常驻
                // 绘制数据（对象表 + 本帧模型/材质环，instance_table 契约）
                let sets = [draw.set0, draw.set1, draw.set2];
                device.cmd_bind_descriptor_sets(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.layout,
                    0,
                    &sets,
                    &[],
                );
                // 动态 viewport/scissor：正高度直出（朝向定案，见 pipeline.rs 模块
                // 注释——光栅级实验 + 官方示例对照截图实证；
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
                // 池整绑一次（offset 0），逐命令在 GPU 侧定位（vertexOffset/
                // firstIndex 已进命令）
                device.cmd_bind_vertex_buffers(frame.command_buffer, 0, &[vertex_buffer], &[0]);
                device.cmd_bind_index_buffer(
                    frame.command_buffer,
                    index_buffer,
                    0,
                    vk::IndexType::UINT32,
                );
                // 一次 indirect 提交：count = 在场行数（CPU 在账信息，非可见集
                // 回读）；不可见行 instanceCount=0，GPU 零工作。stride = 20B
                //（VkDrawIndexedIndirectCommand，命令环步长同源）。
                device.cmd_draw_indexed_indirect(
                    frame.command_buffer,
                    draw.indirect_buffer,
                    0,
                    draw.indirect_count,
                    INDIRECT_COMMAND_STRIDE,
                );
            }

            device.cmd_end_rendering(frame.command_buffer);

            // —— 帧计时 T2：G-buffer 区间结束点位。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 2);
            }

            // —— 附件转只读屏障：albedo/normal（COLOR_ATTACHMENT→SHADER_READ_
            // ONLY，合成采样）与深度（附件→DEPTH_READ_ONLY，合成世界位置采样 +
            // Hi-Z 归约）。消费发生在同一条命令缓冲内，无出场屏障；8.1.3 起
            // 透明层在合成之后把深度转回 ATTACHMENT（附件重绑形态）。
            let attachment_transitions = [
                (
                    draw.gbuffer.albedo_image(slot),
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::ImageAspectFlags::COLOR,
                ),
                (
                    draw.gbuffer.normal_image(slot),
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::ImageAspectFlags::COLOR,
                ),
                (
                    // 8.2.1 matparam：合成 pass 光照累积的 PBR 入参采样面
                    draw.gbuffer.matparam_image(slot),
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::ImageAspectFlags::COLOR,
                ),
                (
                    frame.depth.image,
                    vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                        | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                    vk::ImageAspectFlags::DEPTH,
                ),
            ];
            for (image, old_layout, new_layout, src_access, src_stage, aspect) in
                attachment_transitions
            {
                let to_shader_read = vk::ImageMemoryBarrier::default()
                    .old_layout(old_layout)
                    .new_layout(new_layout)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(aspect)
                            .level_count(1)
                            .layer_count(1),
                    )
                    .src_access_mask(src_access)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                device.cmd_pipeline_barrier(
                    frame.command_buffer,
                    src_stage,
                    vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    slice::from_ref(&to_shader_read),
                );
            }

            // —— 合成 pass（8.1 延迟合成，全屏）：读 G-buffer 出光照——HDR 中间
            // 目标（8.4 起 tone mapping 插在本目标与 swapchain 之间；透明层
            // 8.1.3 起混入同目标）。背景判定（depth==0 = reverse-Z 远平面）在
            // shader 内承接 forward 直出的清屏语义。无深度附件。
            let hdr_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(draw.gbuffer.hdr_view(slot))
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: draw.clear,
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
                    .color_attachments(slice::from_ref(&hdr_attachment)),
            );
            device.cmd_bind_pipeline(
                frame.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                draw.composite_pipeline,
            );
            // set0 = 帧 UBO（与场景管线 set1 同一 layout 对象），set1 = G-buffer
            // 采样集，set2 = cluster 集（8.2.3 光照累积取数）
            let composite_sets = [draw.set1, draw.gbuffer.sampled_set(slot), draw.cluster_set];
            device.cmd_bind_descriptor_sets(
                frame.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                draw.composite_layout,
                0,
                &composite_sets,
                &[],
            );
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
            // 全屏三角形（无顶点 buffer，VS 按 vertex_index 直出）
            device.cmd_draw(frame.command_buffer, 3, 1, 0, 1);
            device.cmd_end_rendering(frame.command_buffer);

            // —— 透明前向层（8.1.3，合成之后同目标接画）：深度附件重绑回
            // ATTACHMENT（READ_ONLY→ATTACHMENT，loadOp LOAD 只测不写），
            // HDR 目标维持 COLOR_ATTACHMENT 布局继续写（loadOp LOAD 混合）。
            if let Some(t) = draw.transparent.as_ref() {
                let depth_to_attachment = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
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
                    // src 含 WRITE：布局迁移本身是写，须覆盖前序 pass 的深度
                    // store（syncval WRITE_AFTER_WRITE 执法面；透明 pass 本身只读）
                    .src_access_mask(
                        vk::AccessFlags::SHADER_READ | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
                    )
                    .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ);
                device.cmd_pipeline_barrier(
                    frame.command_buffer,
                    // src 双阶段：FRAGMENT_SHADER（合成对深度的采样读）+
                    // LATE_FRAGMENT_TESTS（srcAccess 里的深度写访问位的支撑阶段）
                    vk::PipelineStageFlags::FRAGMENT_SHADER
                        | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                    vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                        | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    slice::from_ref(&depth_to_attachment),
                );
                let transparent_attachment = vk::RenderingAttachmentInfo::default()
                    .image_view(draw.gbuffer.hdr_view(slot))
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::LOAD)
                    .store_op(vk::AttachmentStoreOp::STORE);
                let transparent_depth = vk::RenderingAttachmentInfo::default()
                    .image_view(frame.depth.view)
                    .image_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::LOAD)
                    .store_op(vk::AttachmentStoreOp::DONT_CARE);
                device.cmd_begin_rendering(
                    frame.command_buffer,
                    &vk::RenderingInfo::default()
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D::default(),
                            extent,
                        })
                        .layer_count(1)
                        .color_attachments(slice::from_ref(&transparent_attachment))
                        .depth_attachment(&transparent_depth),
                );
                device.cmd_bind_pipeline(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    t.pipeline,
                );
                let t_sets = [draw.set0, draw.set1, draw.set2, t.cluster_set];
                device.cmd_bind_descriptor_sets(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    t.layout,
                    0,
                    &t_sets,
                    &[],
                );
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
                let vertex_buffer = draw.vertex_buffer.ok_or_else(|| {
                    VulkanError::Upload("在场行非空但顶点池缺席——上传链与绘制链的状态矛盾".into())
                })?;
                let index_buffer = draw.index_buffer.ok_or_else(|| {
                    VulkanError::Upload("在场行非空但索引池缺席——上传链与绘制链的状态矛盾".into())
                })?;
                device.cmd_bind_vertex_buffers(frame.command_buffer, 0, &[vertex_buffer], &[0]);
                device.cmd_bind_index_buffer(
                    frame.command_buffer,
                    index_buffer,
                    0,
                    vk::IndexType::UINT32,
                );
                // 一次 indirect 提交消费透明命令环（count = 在场行数上界，
                // 非透明行 instanceCount=0）——透明 draw 调用数恒 ≤ 1（红线同 5.3）
                device.cmd_draw_indexed_indirect(
                    frame.command_buffer,
                    t.t_commands,
                    0,
                    t.command_count,
                    INDIRECT_COMMAND_STRIDE,
                );
                device.cmd_end_rendering(frame.command_buffer);
            }

            // —— HDR 目标转只读（合成 + 透明写 → present blit 采样）。
            let hdr_to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(draw.gbuffer.hdr_image(slot))
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                frame.command_buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                slice::from_ref(&hdr_to_read),
            );

            // —— 帧计时 T3：合成区间（8.1.3 起透明层并入）结束点位。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 3);
            }

            // —— 透明层深度重绑归还：ATTACHMENT→READ_ONLY（Hi-Z 归约采样；
            // transparent None 时深度已在 READ_ONLY，跳过）
            if draw.transparent.is_some() {
                let depth_back_to_read = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(frame.depth_image())
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .level_count(1)
                            .layer_count(1),
                    )
                    // src 含 WRITE：同上——迁移写须覆盖 G-buffer pass 的深度 store
                    .src_access_mask(
                        vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ
                            | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
                    )
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                device.cmd_pipeline_barrier(
                    frame.command_buffer,
                    vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                        | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    slice::from_ref(&depth_back_to_read),
                );
            }

            // —— Hi-Z 归约链（6.1.2）：深度已在 READ_ONLY，逐级 dispatch 重写
            // 本帧槽金字塔（进场/级间屏障在 hiz::record_reduction 内）。
            // 采完即弃，下帧 UNDEFINED 进场照常丢弃。
            if let Some(hiz) = draw.hiz {
                hiz.record_reduction(device, frame.command_buffer, slot);
            }

            // —— 帧计时 T4：Hi-Z 归约区间结束点位。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 4);
            }

            // —— present blit + UI（8.1）：swapchain 只被全屏 blit 写——HDR 中间
            // 目标采样写 swapchain（SRGB view 的 ROP 编码恰好一次）。loadOp
            // DON'T CARE（blit 覆写全部像素，清屏无意义）；UI 同实例接画，
            // raster order 保证 UI 盖在场景之上。overlay 管线只声明 set0（UI
            // 无 UBO），管线切换后 set0 须按 overlay 自己的 layout 对象重绑——
            // 描述符绑定随 layout，不随句柄。
            let blit_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE);
            device.cmd_begin_rendering(
                frame.command_buffer,
                &vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    })
                    .layer_count(1)
                    .color_attachments(slice::from_ref(&blit_attachment)),
            );
            device.cmd_bind_pipeline(
                frame.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                draw.blit_pipeline,
            );
            let blit_sets = [draw.gbuffer.sampled_set(slot)];
            device.cmd_bind_descriptor_sets(
                frame.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                draw.blit_layout,
                0,
                &blit_sets,
                &[],
            );
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
            device.cmd_draw(frame.command_buffer, 3, 1, 0, 1);

            // —— UI 半边：present blit 实例内接画（同 raster order 语义）。
            if let Some(ui) = draw.ui {
                device.cmd_bind_pipeline(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.overlay_pipeline,
                );
                let ui_sets = [draw.set0];
                device.cmd_bind_descriptor_sets(
                    frame.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    draw.overlay_layout,
                    0,
                    &ui_sets,
                    &[],
                );
                // 全屏 viewport：场景分支不必然跑过，UI 段自带 viewport/scissor 状态
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
                device.cmd_bind_vertex_buffers(frame.command_buffer, 0, &[draw.ui_buffers.0], &[0]);
                device.cmd_bind_index_buffer(
                    frame.command_buffer,
                    draw.ui_buffers.1,
                    0,
                    vk::IndexType::UINT32,
                );
                // push 对全帧 UI 恒定（screen_size + 图集双槽位），循环外打包一次
                let ui_push = pack_ui_push(ui.screen_pt, ui.atlas_texture, ui.atlas_sampler);
                for call in &ui.draws {
                    // scissor 逐 clip 设（egui 镶嵌的 clip 即裁剪边界；零尺寸 = 全裁）
                    device.cmd_set_scissor(
                        frame.command_buffer,
                        0,
                        &[vk::Rect2D {
                            offset: vk::Offset2D {
                                x: call.scissor[0] as i32,
                                y: call.scissor[1] as i32,
                            },
                            extent: vk::Extent2D {
                                width: call.scissor[2],
                                height: call.scissor[3],
                            },
                        }],
                    );
                    device.cmd_push_constants(
                        frame.command_buffer,
                        draw.overlay_layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        &ui_push,
                    );
                    device.cmd_draw_indexed(
                        frame.command_buffer,
                        call.index_count,
                        1,
                        call.first_index,
                        call.vertex_base as i32,
                        0,
                    );
                }
            }

            device.cmd_end_rendering(frame.command_buffer);

            // —— 帧计时 T5：present blit + UI 区间结束点位。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 5);
            }

            // —— 出场：颜色画完到 present 引擎接管之间必须收口；PRESENT_SRC 是
            // present 专用布局。深度无出场（storeOp 采完即弃 + 下帧 UNDEFINED 重进）。
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
            // —— 帧计时 T6：全帧区间（T0→T6）结束点位，end_command_buffer 之前。
            if let Some(timer) = draw.timer {
                timer.mark(frame.command_buffer, slot, 6);
            }

            device.end_command_buffer(frame.command_buffer)?;

            // fence 在提交前一刻才重置。此处到 queue_submit 之间没有任何提前
            // return：reset 失败 → fence 保持 signaled，下帧等待照常通过；
            // submit 失败 → 无任何提交负责 signal 这个 fence，帧循环按 Tier② 退出
            //（host.rs），不会带着残局回来等待——"等无人 signal 的 fence"死锁面关闭。
            device.reset_fences(slice::from_ref(&frame.in_flight))?;

            // 等待点语义：
            // ① COLOR_ATTACHMENT_OUTPUT 起才真正需要 image 已到手——之前的顶点处理等
            //    可以和 acquire 重叠；
            // ② ALL_COMMANDS 起等上传票据（timeline，值 = 已发出最高票据）——"上传
            //    完成才可使用"的等待发生在 GPU 执行依赖处，CPU 不先阻塞（探针组 D
            //    同款形状）。该内存依赖同时把 transfer 写对池/贴图的
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
