//! 整帧录制：[`FrameDraw`] 收齐一帧的全部输入，`FramePool::record_frame` 按固定顺序
//! 录完所有 pass 并提交。
//!
//! 全程用 Vulkan 1.3 动态渲染（无 RenderPass/Framebuffer，`loadOp=CLEAR` 即清屏）。每帧布局：
//! - 颜色：acquire 后内容不要 → UNDEFINED→COLOR_ATTACHMENT_OPTIMAL 进场，画完 → PRESENT_SRC_KHR；
//! - 深度：每帧 UNDEFINED 进场（旧内容丢弃，配 loadOp CLEAR）；storeOp STORE 是因为本帧
//!   合成与 Hi-Z 归约还要采样它，采完即弃，不跨帧保留（跨帧的是 Hi-Z 金字塔），也就没有出场屏障。

use std::slice;

use super::frame_pool::FramePool;

use ash::vk;
use bevy::ecs::error::BevyError;

use crate::common::error::{Result, VulkanError};
use crate::common::syntax::ResultTierExt;
use crate::render::core::record::barrier::{ImageBarrier, MemoryBarrierDesc};
use crate::render::core::record::fullscreen::{
    record_fullscreen, record_fullscreen_open, record_fullscreen_sampled, FullscreenPass,
};
use crate::render::core::record::graphics::{
    bind_graphics, bind_vertex_index, color_attachment, full_rect, reverse_z_depth_attachment,
    set_viewport, set_viewport_scissor, PipelineRef,
};
use crate::render::core::binding::set_layout::DescSet;
use crate::render::passes::lighting::cluster::{self, ClusterBuildDraw};
use crate::render::passes::shadow::shadow_build::{self, ShadowBuildDraw};
use crate::render::passes::shadow::shadow_map::{self, ShadowWriteDraw};
use crate::render::passes::transparent::{self, TransparentFrame};
use crate::render::passes::visibility::cull::{self, CullDraw};
use crate::render::passes::visibility::hiz::HizPyramid;
use crate::render::passes::visibility::indirect::{self, IndirectDraw};
use crate::render::passes::visibility::lod::{self, LodDraw};
use crate::render::passes::visibility::occlusion::{self, OcclusionDraw};
use crate::render::GBufferTargets;
use crate::render::passes::overlay::overlay_pipeline::{pack_ui_push, UiPaint};
use crate::render::stats::perf::{GpuTimer, TimestampPoint};
use crate::render::{Context, BLOOM_LEVELS, INDIRECT_COMMAND_SIZE};

/// indirect draw 的命令步长（`VkDrawIndexedIndirectCommand` 20B，与命令环同一常量）。
const INDIRECT_COMMAND_STRIDE: u32 = INDIRECT_COMMAND_SIZE as u32;

/// 一帧录制的全部输入（draw_frame 组装，`record_frame` 消费）。`None` 的可选段整段跳过。
pub struct FrameDraw<'a> {
    pub pipeline: PipelineRef,
    /// bindless 纹理表，整帧绑一次（场景管线 set0 槽）。
    pub bindless_set: DescSet,
    /// 本帧槽的每帧 UBO（场景管线 set1 槽）。
    pub frame_set: DescSet,
    /// 本帧槽的实例表，对象表 + 本帧各环（场景管线 set2 槽）。
    pub instance_set: DescSet,
    pub vertex_buffer: Option<vk::Buffer>,
    pub index_buffer: Option<vk::Buffer>,
    /// 在场行数（= GPU 侧命令条数；0 = 空场，场景绘制整段跳过）。
    pub indirect_count: u32,
    /// 本帧槽命令环（build_indirect 每帧写）。
    pub indirect_buffer: vk::Buffer,
    /// G-buffer 附件 + HDR 中间目标，以及合成/tonemap 用的采样集。
    pub gbuffer: &'a GBufferTargets,
    /// 合成管线（G-buffer 光照累积 → HDR；全屏，无深度/混合）。
    pub composite: PipelineRef,
    /// 光源分簇集，合成与透明前向都绑。
    pub cluster_set: DescSet,
    /// 空场为 None：没东西要画，不用算光源列表。
    pub cluster_build: Option<ClusterBuildDraw>,
    /// tonemap 管线（HDR + Bloom → ACES → LDR）。
    pub tonemap: PipelineRef,
    /// SMAA edge/blend 两 pass（关闭为 None，neighborhood 直通）。
    pub smaa_edge_blend: Option<crate::render::passes::post::smaa::SmaaEdgeBlendDraw>,
    /// neighborhood 混合（终点，写 swapchain，总是录）。
    pub smaa_nb: crate::render::passes::post::smaa::SmaaNbDraw,
    /// tonemap 的 bloom 参数集（总是绑；bloom 关闭时 shader 内不采样）。
    pub bloom_params_set: DescSet,
    /// Bloom 链（关闭为 None）。
    pub bloom: Option<crate::render::passes::post::bloom::BloomFrameDraw>,
    /// overlay 管线。自有 layout（只有 set0），切到它之后 set0 要重绑。
    pub overlay: PipelineRef,
    /// 本帧 UI 绘制产物（无 UI 为 None）。
    pub ui: Option<&'a UiPaint>,
    /// 本帧槽 UI 顶点/索引 buffer（`ui` 为 Some 时本帧已写入）。
    pub ui_buffers: (vk::Buffer, vk::Buffer),
    /// 视锥剔除与阴影视锥标记（两者都关闭或空场为 None）。
    pub frustum_cull: Option<CullDraw>,
    /// indirect 命令组装（空场为 None）。命令写 → 绘制读的屏障在 indirect::record 内。
    pub indirect: Option<IndirectDraw>,
    /// Hi-Z 遮挡测试（关闭/降级/空场为 None）。屏障在 occlusion::record 内。
    pub occlusion: Option<OcclusionDraw>,
    /// GPU LOD 选档（关闭/空场为 None；关闭时 host 已把档位环填 0）。屏障在 lod::record 内。
    pub lod: Option<LodDraw>,
    /// shadow 命令组装（关闭/空场为 None）。屏障在 shadow_build::record 内。
    pub shadow_build: Option<ShadowBuildDraw>,
    /// shadow depth pass（关闭/空场为 None）。
    pub shadow: Option<ShadowWriteDraw>,
    /// shadow 描述符集（合成 pass 总是绑；关闭时 shader 内不采样）。
    pub shadow_set: DescSet,
    /// 级联深度图。shadow 描述符总是绑定，所以每帧都录布局屏障，哪怕 pass 跳过。
    pub shadow_images: [vk::Image; 4],
    /// 透明层：收集/排序/组装 dispatch + 透明绘制（空场为 None）。
    pub transparent: Option<TransparentFrame>,
    /// Hi-Z 金字塔归约，渲染后重写本帧槽；屏障在 hiz::record_reduction 内。
    pub hiz: Option<&'a HizPyramid>,
    /// 图形提交在 GPU 侧等上传 timeline ≥ 此值（0 = 从未上传，不等）。这个等待同时保证 transfer
    /// 写的跨队列可见性（池/贴图 CONCURRENT 共享，没有所有权屏障）。
    pub wait_ticket: u64,
    pub ticket_semaphore: vk::Semaphore,
    /// 清屏颜色（线性；SRGB view 写出时编码）。
    pub clear: [f32; 4],
    /// GPU 帧计时：帧首 reset + 原点，段界打点见 [`TimestampPoint`]；None = 队列族不支持
    /// timestamp。
    pub timer: Option<&'a GpuTimer>,
}

impl FramePool {
    /// 录制并提交一帧：进场屏障（颜色+深度+G-buffer）→ compute 段（剔除/遮挡/LOD/indirect 与
    /// shadow 命令组装/光源列表/透明）→ shadow depth → G-buffer → 合成 + 透明层 → Hi-Z 归约 →
    /// Bloom → tonemap → SMAA → neighborhood + UI → 出场屏障 → queue_submit。提交在 GPU 侧等
    /// image_available 与上传票据，完成时 signal render_finished 与 fence。`render_finished`
    /// 由调用方按 acquire 的 image index 从 Swapchain 取，不按帧槽轮转。
    /// 录制或提交任一步失败 = 渲染链无法继续，按致命错误冒泡。
    pub fn record_frame(
        &self,
        ctx: &Context,
        render_finished: vk::Semaphore,
        swapchain_image: vk::Image,
        swapchain_view: vk::ImageView,
        extent: vk::Extent2D,
        draw: &FrameDraw<'_>,
    ) -> Result<(), BevyError> {
        // # Safety:前提由帧循环在调用前建立（draw_frame 先 wait_for_slot 再 acquire_index）
        unsafe {
            self.record_frame_steps(ctx, render_finished, swapchain_image, swapchain_view, extent, draw)
        }
        .or_fatal("录制/提交失败，渲染链无法继续")
    }

    /// 录制体九段，顺序固定：
    /// ① 进场（计时 + 进场屏障 + 透明排序键预填）
    /// ② compute 前置
    /// ③ shadow depth
    /// ④ G-buffer
    /// ⑤ 合成 + 透明前向
    /// ⑥ Hi-Z 归约
    /// ⑦ Bloom
    /// ⑧ tonemap/SMAA/neighborhood + UI
    /// ⑨ 出场 + 提交。段界 = image layout 迁移点，计时打点挂在段界；
    /// ②~⑧ 一段对应一个业务模块，段内输入全部来自 `draw`，段间衔接只靠布局屏障；
    /// ①⑨ 是槽基建段（cb/fence/等待列表），消费槽私有物，不属于业务模块。
    /// 机制正本见笔记《整帧录制流程》。
    ///
    /// # Safety
    /// 本槽 fence 已由 `wait_for_slot` 等完（cb 与槽私有附件无 GPU 在用）；`swapchain_image`/
    /// `swapchain_view`/`render_finished` 属于同一次 `acquire_index` 的 image index，且 acquire
    /// 用的是本槽 `image_available`；`draw` 内句柄均按当前 slot 取且录制期间存活。
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn record_frame_steps(
        &self,
        ctx: &Context,
        render_finished: vk::Semaphore,
        swapchain_image: vk::Image,
        swapchain_view: vk::ImageView,
        extent: vk::Extent2D,
        draw: &FrameDraw<'_>,
    ) -> Result<()> {
        let frame = &self.frames[self.current];
        let device = &self.device;
        let slot = self.current;
        let cb = frame.command_buffer;
        let mark = |point: TimestampPoint| {
            if let Some(timer) = draw.timer {
                timer.mark(cb, slot, point);
            }
        };

        // ---- ① 进场：cb 开始 → 帧计时 → 三组进场屏障 → 透明排序键预填 ----
        device.begin_command_buffer(
            cb,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        // 帧计时：重置 query pool + 原点
        if let Some(timer) = draw.timer {
            timer.reset_and_start(cb, slot);
        }

        // 进场屏障一：颜色布局迁移。迁移本身是写，必须排在 acquire 对 swapchain image 的读之后：
        // 提交在 COLOR_ATTACHMENT_OUTPUT 等 image_available，所以 srcStage 也取这里；
        // dst_access 含 COLOR_ATTACHMENT_WRITE，衔接后面的 loadOp 清屏。
        ImageBarrier::discard_to_color(
            swapchain_image,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        )
        .record(device, cb);

        // 进场屏障二：深度 UNDEFINED→附件。src access 为空是合法的：本槽深度的上一轮使用
        // 已被 wait_for_slot 的 fence 等完，录制时没有并发引用。
        ImageBarrier::discard_to_depth_attachment(
            frame.depth.image,
            vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
        )
        .record(device, cb);

        // 进场屏障三：G-buffer 三附件 + HDR 目标 UNDEFINED→附件。每帧 loadOp CLEAR 覆写全部
        // 像素，本槽上一轮使用已被 fence 等完（同进场屏障二），旧内容不要。
        for gbuffer_image in [
            draw.gbuffer.albedo_image(slot),
            draw.gbuffer.normal_image(slot),
            draw.gbuffer.matparam_image(slot),
            draw.gbuffer.hdr_image(slot),
        ] {
            ImageBarrier::discard_to_color(
                gbuffer_image,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            )
            .record(device, cb);
        }

        // 贴图不需要进场屏障：图像 CONCURRENT 双族共享，不做所有权转移；上传后的
        // TRANSFER_DST→SHADER_READ_ONLY 在 uploader 的 transfer 提交内完成，跨队列可见性
        // 由提交时的票据等待保证（EXCLUSIVE 方案的对照实验在 examples/image_probe 组 C）。

        // 透明排序键预填 -inf：bitonic 全容量升序排序把真实条目推到尾部（gather 反向取），
        // 空位的键必须是 -inf。要在所有 compute dispatch 之前。
        if let Some(t) = draw.transparent.as_ref() {
            device.cmd_fill_buffer(cb, t.t_keys, 0, vk::WHOLE_SIZE, f32::NEG_INFINITY.to_bits());
            // 填充是 transfer 写，后续 sort/collect 的 compute 读写前必须可见
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::TRANSFER,
                src_access: vk::AccessFlags::TRANSFER_WRITE,
                dst_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                dst_access: vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            }
            .record(device, cb);
        }

        // ---- ② compute 前置：剔除 → 遮挡 → LOD → indirect 组装 → shadow 组装 → 光源列表 → 透明层 ----
        // 顺序即依赖，各步之间的屏障都在各自的 record 内。
        if let Some(frustum_cull) = &draw.frustum_cull {
            cull::record(device, cb, draw.instance_set, draw.shadow_set, frustum_cull);
        }

        // 读视锥标记与两帧前的 Hi-Z 金字塔
        if let Some(occlusion) = &draw.occlusion {
            occlusion::record(device, cb, draw.instance_set, occlusion);
        }

        if let Some(lod) = &draw.lod {
            lod::record(device, cb, draw.instance_set, lod);
        }

        if let Some(indirect) = &draw.indirect {
            indirect::record(device, cb, draw.instance_set, indirect);
        }

        if let Some(sb) = &draw.shadow_build {
            shadow_build::record(device, cb, draw.instance_set, draw.shadow_set, sb);
        }

        // 光源列表必须在合成和透明前向之前算好
        if let Some(build) = &draw.cluster_build {
            cluster::record_cluster_build(device, cb, draw.cluster_set, build);
        }

        // 透明层：collect → sort → gather
        if let Some(t) = draw.transparent.as_ref() {
            if let Some(collect) = &t.collect {
                transparent::record_transparent_collect(
                    device,
                    cb,
                    draw.instance_set,
                    draw.cluster_set,
                    t.transparent_set,
                    collect,
                );
            }
            // 固定容量数组每个非空帧都重排，保证本帧新增的透明行不会以未排序状态进 gather
            transparent::record_transparent_sort(device, cb, t.transparent_set, &t.sort);
            if let Some(gather) = &t.gather {
                transparent::record_transparent_gather(
                    device,
                    cb,
                    draw.instance_set,
                    t.transparent_set,
                    gather,
                );
            }
        }

        mark(TimestampPoint::Compute);

        // ---- ③ shadow depth：进场 → 逐级联 indirect draw → 出场 ----
        // 进出屏障每帧都录（合成 pass 的 shadow 描述符总是绑定）。
        shadow_map::record(device, cb, &draw.shadow_images, draw.shadow.as_ref());

        mark(TimestampPoint::Shadow);

        // ---- ④ G-buffer：不透明物体写三色附件 + 深度 ----
        // 全部 loadOp CLEAR（背景像素内容无意义，合成按 depth==0 判背景）；深度清 0（reverse-Z
        // 远平面），STORE 供合成与 Hi-Z 采样。
        // 附件数必须与管线声明一致（VUID-06179：colorAttachmentCount 相等）
        let gbuffer_attachments = [
            color_attachment(draw.gbuffer.albedo_view(slot), Some([0.0, 0.0, 0.0, 0.0])),
            color_attachment(draw.gbuffer.normal_view(slot), Some([0.0, 0.0, 0.0, 0.0])),
            color_attachment(draw.gbuffer.matparam_view(slot), Some([0.0, 0.0, 0.0, 1.0])),
        ];
        let depth_attachment = reverse_z_depth_attachment(frame.depth.view);
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(full_rect(extent))
                .layer_count(1)
                .color_attachments(&gbuffer_attachments)
                .depth_attachment(&depth_attachment),
        );

        // 场景绘制：一次 cmd_draw_indexed_indirect 消费命令环，draw 调用数不随对象数增长。
        // 实例槽号经 firstInstance 进 VS（按 instance_index 读 set2）；不可见行 instanceCount=0。
        if draw.indirect_count > 0 {
            let (vertex_buffer, index_buffer) = draw.pool_buffers()?;
            bind_graphics(device, cb, &draw.pipeline, &[draw.bindless_set, draw.frame_set, draw.instance_set]);
            // 坑：viewport 正高度，原因见 set_viewport
            set_viewport_scissor(device, cb, extent);
            // 整池绑一次，每条命令自带 vertexOffset/firstIndex
            bind_vertex_index(device, cb, vertex_buffer, index_buffer);
            device.cmd_draw_indexed_indirect(
                cb,
                draw.indirect_buffer,
                0,
                draw.indirect_count,
                INDIRECT_COMMAND_STRIDE,
            );
        }

        device.cmd_end_rendering(cb);

        mark(TimestampPoint::GBuffer);

        // ---- ⑤ 合成 + 透明前向：G-buffer 转只读 → 合成光照写 HDR → 透明层混合 → HDR 转只读 ----
        // G-buffer 三色附件 → SHADER_READ_ONLY，深度 → DEPTH_READ_ONLY，供合成采样；dst 含
        // COMPUTE 是因为 Hi-Z 归约也读深度。透明层会在合成后把深度再转回附件。
        let color_to_read = |image| ImageBarrier {
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::COMPUTE_SHADER,
            ..ImageBarrier::color_to_sampled(image)
        };
        for barrier in [
            color_to_read(draw.gbuffer.albedo_image(slot)),
            color_to_read(draw.gbuffer.normal_image(slot)),
            // matparam：合成 pass 光照累积的 PBR 入参采样面
            color_to_read(draw.gbuffer.matparam_image(slot)),
            ImageBarrier::depth_attachment_to_read_only(frame.depth.image),
        ] {
            barrier.record(device, cb);
        }

        // 合成 pass（全屏，无深度附件）：读 G-buffer 算光照写 HDR 目标；depth==0（reverse-Z
        // 远平面）的像素当背景。set0 帧 UBO（与场景管线 set1 同一 layout）、set1 G-buffer 采样集、
        // set2 光源分簇、set3 shadow。
        record_fullscreen(device, cb, &FullscreenPass {
            view: draw.gbuffer.hdr_view(slot),
            extent,
            clear: Some(draw.clear),
            pipeline: draw.composite,
            sets: &[draw.frame_set, draw.gbuffer.sampled_set(slot), draw.cluster_set, draw.shadow_set],
        });

        // 透明前向层：合成之后在同一 HDR 目标上 LOAD + 混合，深度转回附件只测不写。
        if let Some(t) = draw.transparent.as_ref() {
            // 两次动态渲染之间没有光栅序保证，合成写 HDR → 透明混合读写必须显式屏障
            let hdr_write_to_blend = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(
                    vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                );
            ImageBarrier::depth_read_only_to_attachment(frame.depth.image)
                .record_with_memory(device, cb, slice::from_ref(&hdr_write_to_blend));
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
                cb,
                &vk::RenderingInfo::default()
                    .render_area(full_rect(extent))
                    .layer_count(1)
                    .color_attachments(slice::from_ref(&transparent_attachment))
                    .depth_attachment(&transparent_depth),
            );
            bind_graphics(device, cb, &t.pipeline, &[draw.bindless_set, draw.frame_set, draw.instance_set, draw.cluster_set]);
            set_viewport_scissor(device, cb, extent);
            let (vertex_buffer, index_buffer) = draw.pool_buffers()?;
            bind_vertex_index(device, cb, vertex_buffer, index_buffer);
            // 一次 indirect 消费透明命令环（非透明行 instanceCount=0）
            device.cmd_draw_indexed_indirect(
                cb,
                t.t_commands,
                0,
                t.command_count,
                INDIRECT_COMMAND_STRIDE,
            );
            device.cmd_end_rendering(cb);
        }

        // HDR 目标 → 只读，供 Bloom 与 tonemap 采样
        ImageBarrier::color_to_sampled(draw.gbuffer.hdr_image(slot)).record(device, cb);

        mark(TimestampPoint::Lighting);

        // ---- ⑥ Hi-Z：深度转回只读 → 逐级归约金字塔 ----
        // 透明层用过的深度转回只读，供 Hi-Z 归约（没有透明层时深度一直是只读）
        if draw.transparent.is_some() {
            // 透明 pass 测深读过（src 加 READ）；dst 只等 compute（Hi-Z 归约读）。
            // 含 WRITE：迁移要排在 G-buffer pass 的深度 store 之后
            ImageBarrier {
                src_access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
                dst_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                ..ImageBarrier::depth_attachment_to_read_only(frame.depth.image)
            }
            .record(device, cb);
        }

        // Hi-Z 归约：逐级重写本帧槽金字塔（屏障在 record_reduction 内）
        if let Some(hiz) = draw.hiz {
            hiz.record_reduction(device, cb, slot);
        }

        mark(TimestampPoint::HiZ);

        // ---- ⑦ Bloom：prefilter → 逐级 down → 逐级 up ----
        // prefilter 读 HDR 提取亮部写 down0；up 上采样下一级并叠加同级 down。各级目标每次
        // UNDEFINED→COLOR 进、→SHADER_READ_ONLY 出。
        if let Some(bloom) = &draw.bloom {
            let prefilter = (
                bloom.images_down[0],
                bloom.views_down[0],
                bloom.extents[0],
                bloom.prefilter,
                bloom.gbuffer_set,
            );
            let downs = (1..BLOOM_LEVELS).map(|i| {
                (
                    bloom.images_down[i],
                    bloom.views_down[i],
                    bloom.extents[i],
                    bloom.down,
                    bloom.down_sets[i - 1],
                )
            });
            let ups = (0..BLOOM_LEVELS - 1).rev().map(|i| {
                (
                    bloom.images_up[i],
                    bloom.views_up[i],
                    bloom.extents[i],
                    bloom.up,
                    bloom.up_sets[i],
                )
            });
            for (target, target_view, target_extent, pipeline, set) in
                std::iter::once(prefilter).chain(downs).chain(ups)
            {
                record_fullscreen_sampled(
                    device,
                    cb,
                    target,
                    // bloom 目标不分帧槽：等上一帧的片元采样读完再覆写（WAR）
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    &FullscreenPass {
                        view: target_view,
                        extent: target_extent,
                        clear: None,
                        pipeline,
                        sets: &[set],
                    },
                );
            }
        }

        // ---- ⑧ 后链 + UI：tonemap → SMAA edge/blend → neighborhood + UI ----
        // tonemap（HDR + Bloom → ACES → LDR）：LDR 是 UNORM 线性存储，sRGB 编码只在最后写
        // swapchain 的 SRGB view 时做一次。坑：LDR 上一轮停在 SHADER_READ_ONLY，不录 UNDEFINED
        // 进场屏障会触发 VUID-09592 布局不符。出场转只读供 SMAA 采样。
        record_fullscreen_sampled(
            device,
            cb,
            draw.smaa_nb.ldr_image,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            &FullscreenPass {
                view: draw.smaa_nb.ldr_view,
                extent: draw.smaa_nb.ldr_extent,
                clear: None,
                pipeline: draw.tonemap,
                sets: &[draw.frame_set, draw.gbuffer.sampled_set(slot), draw.bloom_params_set],
            },
        );

        // SMAA edge → blend（关闭为 None）。中间目标按帧槽分配，同槽上一轮已被 fence 等完，
        // UNDEFINED 进场即可。
        if let Some(smaa) = &draw.smaa_edge_blend {
            for (target, target_view, rec, set) in [
                (smaa.edges_image, smaa.edges_view, smaa.edge, smaa.edge_set),
                (smaa.blend_image, smaa.blend_view, smaa.blend, smaa.blend_set),
            ] {
                record_fullscreen_sampled(
                    device,
                    cb,
                    target,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    &FullscreenPass {
                        view: target_view,
                        extent: smaa.extent,
                        clear: None,
                        pipeline: rec,
                        sets: &[set],
                    },
                );
            }
        }

        // neighborhood 混合（终点）：LDR + blend → swapchain（SMAA 关闭时 shader 内直通）。
        // UI 在同一次渲染里接着画，按光栅序盖在场景之上。
        record_fullscreen_open(device, cb, &FullscreenPass {
            view: swapchain_view,
            extent,
            clear: None,
            pipeline: draw.smaa_nb.nb,
            sets: &[draw.smaa_nb.nb_set],
        });

        // UI：接在 neighborhood 的同一次渲染里画
        if let Some(ui) = draw.ui {
            // 描述符绑定跟随 pipeline layout：切到 overlay 管线后 set0 要按它的 layout 重绑
            bind_graphics(device, cb, &draw.overlay, &[draw.bindless_set]);
            // 场景分支不一定跑过，UI 自己设 viewport
            set_viewport(device, cb, extent);
            bind_vertex_index(device, cb, draw.ui_buffers.0, draw.ui_buffers.1);
            // push 常量整帧不变，循环外打包一次
            let ui_push = pack_ui_push(ui.screen_pt, ui.atlas_texture, ui.atlas_sampler);
            for call in &ui.draws {
                // 每个 clip 设一次 scissor（零尺寸 = 全裁）
                device.cmd_set_scissor(
                    cb,
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
                    cb,
                    draw.overlay.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &ui_push,
                );
                device.cmd_draw_indexed(
                    cb,
                    call.index_count,
                    1,
                    call.first_index,
                    call.vertex_base as i32,
                    0,
                );
            }
        }

        device.cmd_end_rendering(cb);

        mark(TimestampPoint::PostUi);

        // ---- ⑨ 出场 + 提交：swapchain → PRESENT_SRC_KHR → 结束 cb → 重置 fence → 提交 ----
        ImageBarrier {
            image: swapchain_image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            src_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            src_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            dst_access: vk::AccessFlags::empty(),
        }
        .record(device, cb);

        mark(TimestampPoint::FrameEnd);

        device.end_command_buffer(cb)?;

        // 坑：fence 在提交前一刻才重置，这里到 queue_submit 之间不能有提前 return。reset 失败
        // 时 fence 仍 signaled，下帧照常通过；submit 失败则没人 signal 它，帧循环直接报错退出，
        // 不会回来死等（见 `wait_for_slot`）。
        device.reset_fences(slice::from_ref(&frame.in_flight))?;

        // 等待点：
        // ① image_available 在 COLOR_ATTACHMENT_OUTPUT 才需要，之前的工作可与 acquire 重叠；
        // ② 上传票据（timeline）在 ALL_COMMANDS 等，CPU 不阻塞；这个内存依赖同时保证
        //    transfer 写池/贴图的跨队列可见性（CONCURRENT 共享没有所有权屏障，全靠这里）。
        // 票据 0 表示从未上传，不能等 0 值（VUID-03242），只等 acquire。fallback 白图在初始化时
        // 就上传了，实际走不到这个分支。
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
        Ok(())
    }
}

impl FrameDraw<'_> {
    /// 顶点/索引池句柄。有在场行时池必然已建，缺席说明上传与绘制状态矛盾，报错退出而不是
    /// 静默跳过整个场景。
    fn pool_buffers(&self) -> Result<(vk::Buffer, vk::Buffer)> {
        let vertex_buffer = self.vertex_buffer.ok_or_else(|| {
            VulkanError::Contract("在场行非空但顶点池缺席——上传链与绘制链的状态矛盾".into())
        })?;
        let index_buffer = self.index_buffer.ok_or_else(|| {
            VulkanError::Contract("在场行非空但索引池缺席——上传链与绘制链的状态矛盾".into())
        })?;
        Ok((vertex_buffer, index_buffer))
    }
}
