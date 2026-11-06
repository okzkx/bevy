//! 整帧录制：`FramePool::record_frame` 按九段固定顺序录完所有 pass 并提交。功能输入在
//! [`RenderFeatures`](super::render_features::RenderFeatures)，场景资源在
//! [`SceneResources`](super::scene_resources::SceneResources)，本模块只管录制编排。
//!
//! 全程用 Vulkan 1.3 动态渲染（无 RenderPass/Framebuffer，`loadOp=CLEAR` 即清屏）。每帧布局：
//! - 颜色：acquire 后内容不要 → UNDEFINED→COLOR_ATTACHMENT_OPTIMAL 进场，画完 → PRESENT_SRC_KHR；
//! - 深度：每帧 UNDEFINED 进场（旧内容丢弃，配 loadOp CLEAR）；storeOp STORE 是因为本帧
//!   合成与 Hi-Z 归约还要采样它，采完即弃，不跨帧保留（跨帧的是 Hi-Z 金字塔），也就没有出场屏障。

use std::slice;

use super::frame_pool::{FramePool, SubmitWait};
use super::render_features::RenderFeatures;
use super::scene_resources::SceneResources;

use ash::vk;
use bevy::ecs::error::BevyError;

use crate::common::error::Result;
use crate::common::syntax::ResultTierExt;
use crate::render::core::record::barrier::{ImageBarrier, MemoryBarrierDesc};
use crate::render::core::record::fullscreen::FullscreenPass;
use crate::render::core::record::graphics::{
    bind_graphics, bind_vertex_index, color_attachment, full_rect, load_color_attachment,
    load_depth_attachment, reverse_z_depth_attachment, set_viewport_scissor,
};
use crate::render::stats::perf::{FrameMark, TimestampPoint};
use crate::render::{PresentTarget, INDIRECT_COMMAND_SIZE};

/// indirect draw 的命令步长（`VkDrawIndexedIndirectCommand` 20B，与命令环同一常量）。
const INDIRECT_COMMAND_STRIDE: u32 = INDIRECT_COMMAND_SIZE as u32;

impl FramePool {
    /// 录制并提交一帧：进场屏障（颜色+深度+G-buffer）→ compute 段（剔除/遮挡/LOD/indirect 与
    /// shadow 命令组装/光源列表/透明）→ shadow depth → G-buffer → 合成 + 透明层 → Hi-Z 归约 →
    /// Bloom → tonemap → SMAA → neighborhood + UI → 出场屏障 → queue_submit。提交在 GPU 侧等
    /// image_available 与上传票据，完成时 signal `target.render_finished` 与 fence。`target`
    /// 四件套（image/view/extent/render_finished）是本次 acquire 的产物，按 image index 取，
    /// 不按帧槽轮转。功能门控走 `feature`，场景资源与规格走 `res`。
    /// 录制或提交任一步失败 = 渲染链无法继续，按致命错误冒泡。
    pub fn record_frame(
        &self,
        target: &PresentTarget,
        feature: &RenderFeatures<'_>,
        res: &SceneResources<'_>,
    ) -> Result<(), BevyError> {
        // # Safety:前提由帧循环在调用前建立（draw_frame 先 wait_for_slot 再 acquire_index）
        unsafe {
            self.record_frame_steps(target, feature, res)
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
    /// ⑧ tonemap/SMAA/neighborhood + UI（⑦⑧ 录制收进 [`PostChainDraw::record`](crate::render::passes::post::post_chain::PostChainDraw::record)，
    /// UI 接在 neighborhood 的同一次渲染里）
    /// ⑨ 出场 + 提交。段界 = image layout 迁移点，计时打点挂在段界；
    /// ②~⑧ 一段对应一个业务模块，段内输入全部来自 `feature`（功能门控）与 `res`（场景资源），
/// 间衔接只靠布局屏障；
    /// ①⑨ 是槽基建段（cb/fence/等待列表），消费槽私有物，不属于业务模块。
    /// 机制正本见笔记《整帧录制流程》。
    ///
    /// # Safety
    /// 本槽 fence 已由 `wait_for_slot` 等完（cb 与槽私有附件无 GPU 在用）；`target` 四件套
    /// 属于同一次 `acquire_index` 的 image index，且 acquire 用的是本槽 `image_available`；
    /// `feature` 与 `res` 内句柄均按当前 slot 取且录制期间存活。
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn record_frame_steps(
        &self,
        target: &PresentTarget,
        feature: &RenderFeatures<'_>,
        res: &SceneResources<'_>,
    ) -> Result<()> {
        let frame = &self.frames[self.current];
        let device = &self.device;
        let slot = self.current;
        let cb = frame.command_buffer;
        let extent = target.extent;
        let mark = FrameMark { cb, slot, timer: feature.timer };

        // ---- ① 进场：cb 开始 → 帧计时 → 三组进场屏障 → 透明排序键预填 ----
        device.begin_command_buffer(
            cb,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        // 帧计时：重置 query pool + 原点
        if let Some(timer) = feature.timer {
            timer.reset_and_start(cb, slot);
        }

        // 进场屏障一：颜色布局迁移。迁移本身是写，必须排在 acquire 对 swapchain image 的读之后：
        // 提交在 COLOR_ATTACHMENT_OUTPUT 等 image_available，所以 srcStage 也取这里；
        // dst_access 含 COLOR_ATTACHMENT_WRITE，衔接后面的 loadOp 清屏。
        ImageBarrier::discard_to_color(
            target.image,
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
            res.gbuffer.albedo_image(slot),
            res.gbuffer.normal_image(slot),
            res.gbuffer.matparam_image(slot),
            res.gbuffer.hdr_image(slot),
        ] {
            ImageBarrier::discard_to_color(
                gbuffer_image,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            )
            .record(device, cb);
        }

        // 贴图不需要进场屏障：图像 CONCURRENT 双族共享，不做所有权转移；上传后的
        // TRANSFER_DST→SHADER_READ_ONLY 在 uploader 的 transfer 提交内完成，跨队列可见性
        // 由提交时的票据等待保证（EXCLUSIVE 方案需逐图所有权转移，CONCURRENT 免除）。

        // 透明排序键预填 -inf：bitonic 全容量升序排序把真实条目推到尾部（gather 反向取），
        // 空位的键必须是 -inf。要在所有 compute dispatch 之前。
        if let Some(t) = feature.transparent.as_ref() {
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
        if let Some(frustum_cull) = &feature.frustum_cull {
            frustum_cull.record(device, cb, res.instance_set, feature.shadow.set);
        }

        // 读视锥标记与两帧前的 Hi-Z 金字塔
        feature.hiz.record_occlusion(device, cb, res.instance_set);

        if let Some(lod) = &feature.lod {
            lod.record(device, cb, res.instance_set);
        }

        if let Some(indirect) = &feature.indirect {
            indirect.record(device, cb, res.instance_set);
        }

        feature.shadow.record_build(device, cb, res.instance_set);

        feature.cluster.record_build(device, cb);

        // 透明层：collect → sort → gather
        if let Some(t) = feature.transparent.as_ref() {
            if let Some(collect) = &t.collect {
                collect.record(device, cb, res.instance_set, feature.cluster.set, t.transparent_set);
            }

            // 固定容量数组每个非空帧都重排，保证本帧新增的透明行不会以未排序状态进 gather
            t.sort.record(device, cb, t.transparent_set);

            if let Some(gather) = &t.gather {
                gather.record(device, cb, res.instance_set, t.transparent_set);
            }
        }

        mark.mark(TimestampPoint::Compute);

        // ---- ③ shadow depth：进场 → 逐级联 indirect draw → 出场 ----
        // 进出屏障每帧都录（合成 pass 的 shadow 描述符总是绑定）。
        feature.shadow.record_depth(device, cb);

        mark.mark(TimestampPoint::Shadow);

        // ---- ④ G-buffer：不透明物体写三色附件 + 深度 ----
        // 全部 loadOp CLEAR（背景像素内容无意义，合成按 depth==0 判背景）；深度清 0（reverse-Z
        // 远平面），STORE 供合成与 Hi-Z 采样。
        // 附件数必须与管线声明一致（VUID-06179：colorAttachmentCount 相等）
        let gbuffer_attachments = [
            color_attachment(res.gbuffer.albedo_view(slot), Some([0.0, 0.0, 0.0, 0.0])),
            color_attachment(res.gbuffer.normal_view(slot), Some([0.0, 0.0, 0.0, 0.0])),
            color_attachment(res.gbuffer.matparam_view(slot), Some([0.0, 0.0, 0.0, 1.0])),
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
        if res.indirect_count > 0 {
            let (vertex_buffer, index_buffer) = res.pool_buffers()?;
            bind_graphics(device, cb, &res.pipeline, &[res.bindless_set, res.frame_set, res.instance_set]);
            // 坑：viewport 正高度，原因见 set_viewport
            set_viewport_scissor(device, cb, extent);
            // 整池绑一次，每条命令自带 vertexOffset/firstIndex
            bind_vertex_index(device, cb, vertex_buffer, index_buffer);
            device.cmd_draw_indexed_indirect(
                cb,
                res.indirect_buffer,
                0,
                res.indirect_count,
                INDIRECT_COMMAND_STRIDE,
            );
        }

        device.cmd_end_rendering(cb);

        mark.mark(TimestampPoint::GBuffer);

        // ---- ⑤ 合成 + 透明前向：G-buffer 转只读 → 合成光照写 HDR → 透明层混合 → HDR 转只读 ----
        // G-buffer 三色附件 → SHADER_READ_ONLY，深度 → DEPTH_READ_ONLY，供合成采样；dst 含
        // COMPUTE 是因为 Hi-Z 归约也读深度。透明层会在合成后把深度再转回附件。
        let color_to_read = |image| ImageBarrier {
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::COMPUTE_SHADER,
            ..ImageBarrier::color_to_sampled(image)
        };
        for barrier in [
            color_to_read(res.gbuffer.albedo_image(slot)),
            color_to_read(res.gbuffer.normal_image(slot)),
            // matparam：合成 pass 光照累积的 PBR 入参采样面
            color_to_read(res.gbuffer.matparam_image(slot)),
            ImageBarrier::depth_attachment_to_read_only(frame.depth.image),
        ] {
            barrier.record(device, cb);
        }

        // 合成 pass（全屏，无深度附件）：读 G-buffer 算光照写 HDR 目标；depth==0（reverse-Z
        // 远平面）的像素当背景。set0 帧 UBO（与场景管线 set1 同一 layout）、set1 G-buffer 采样集、
        // set2 光源分簇、set3 shadow。
        FullscreenPass {
            view: res.gbuffer.hdr_view(slot),
            extent,
            // 清屏色按线性域给值——HDR 目标的 SRGB view 在 ROP 写出时才编码，别拿 sRGB 十六进制直接换算。
            clear: Some([0.02, 0.06, 0.14, 1.0]),
            pipeline: res.composite,
            sets: &[res.frame_set, res.gbuffer.sampled_set(slot), feature.cluster.set, feature.shadow.set],
        }
        .record(device, cb);

        // 透明前向层：合成之后在同一 HDR 目标上 LOAD + 混合。两次动态渲染之间没有光栅
        // 序保证，合成写 HDR → 透明混合读写必须显式内存屏障；深度在本段转回附件（只测
        // 不写），⑥ 段 Hi-Z 归约再转回只读。
        if let Some(t) = feature.transparent.as_ref() {
            let (vertex_buffer, index_buffer) = res.pool_buffers()?;
            // 合成写 HDR → 透明混合读写（两次动态渲染之间没有光栅序保证），与深度
            // 转回附件合成同一次 vkCmdPipelineBarrier
            ImageBarrier::depth_read_only_to_attachment(frame.depth.image).record_with_memory(
                device,
                cb,
                &[MemoryBarrierDesc {
                    src_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    src_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    dst_access: vk::AccessFlags::COLOR_ATTACHMENT_READ
                        | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                }],
            );
            let transparent_attachment = load_color_attachment(res.gbuffer.hdr_view(slot));
            let transparent_depth = load_depth_attachment(frame.depth.view);
            device.cmd_begin_rendering(
                cb,
                &vk::RenderingInfo::default()
                    .render_area(full_rect(extent))
                    .layer_count(1)
                    .color_attachments(std::slice::from_ref(&transparent_attachment))
                    .depth_attachment(&transparent_depth),
            );
            bind_graphics(
                device,
                cb,
                &t.pipeline,
                &[res.bindless_set, res.frame_set, res.instance_set, feature.cluster.set],
            );
            set_viewport_scissor(device, cb, extent);
            bind_vertex_index(device, cb, vertex_buffer, index_buffer);
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

        // HDR 目标 → 只读，供 Bloom 与 tonemap 采样
        ImageBarrier::color_to_sampled(res.gbuffer.hdr_image(slot)).record(device, cb);

        mark.mark(TimestampPoint::Lighting);

        // ---- ⑥ Hi-Z：深度转回只读 → 逐级归约金字塔 ----
        // 透明层用过的深度转回只读，供 Hi-Z 归约（没有透明层时深度一直是只读）
        if feature.transparent.is_some() {
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
        feature.hiz.record_reduction(device, cb, slot);

        mark.mark(TimestampPoint::HiZ);

        // ---- ⑦⑧ 后链 + UI：Bloom → tonemap → SMAA → neighborhood + UI ----
        // ⑦⑧ 录制收进 PostChainDraw::record；neighborhood 开渲染不关，UI 接在同一次
        // 渲染里按光栅序盖在场景之上。
        feature.post.record(
            device,
            cb,
            res.frame_set,
            res.gbuffer.sampled_set(slot),
            target.view,
            extent,
        );

        // UI
        if let Some(ui) = &feature.ui {
            ui.record(device, cb, res.bindless_set, extent);
        }

        device.cmd_end_rendering(cb);

        mark.mark(TimestampPoint::PostUi);

        // ---- ⑨ 出场 + 提交：swapchain → PRESENT_SRC_KHR → 结束 cb → 重置 fence → 提交 ----
        ImageBarrier {
            image: target.image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            src_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            src_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            dst_access: vk::AccessFlags::empty(),
        }
        .record(device, cb);

        mark.mark(TimestampPoint::FrameEnd);

        device.end_command_buffer(cb)?;

        // 坑：fence 在提交前一刻才重置，这里到 queue_submit 之间不能有提前 return。reset 失败
        // 时 fence 仍 signaled，下帧照常通过；submit 失败则没人 signal 它，帧循环直接报错退出，
        // 不会回来死等（见 `wait_for_slot`）。
        device.reset_fences(slice::from_ref(&frame.in_flight))?;

        // 等待点：① image_available 在 COLOR_ATTACHMENT_OUTPUT 才需要，之前的工作可与
        // acquire 重叠；② 上传票据（timeline）在 ALL_COMMANDS 等，CPU 不阻塞；这个内存依赖
        // 同时保证 transfer 写池/贴图的跨队列可见性（CONCURRENT 共享没有所有权屏障，全靠
        // 这里）。
        let mut waits = vec![SubmitWait {
            semaphore: frame.image_available,
            dst_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            value: 0,
        }];
        waits.extend(feature.upload_wait.entry());
        let (wait_semaphores, wait_dst_stage, wait_values): (
            Vec<vk::Semaphore>,
            Vec<vk::PipelineStageFlags>,
            Vec<u64>,
        ) = waits
            .into_iter()
            .map(|w| (w.semaphore, w.dst_stage, w.value))
            .collect();
        let mut timeline_submit =
            vk::TimelineSemaphoreSubmitInfo::default().wait_semaphore_values(&wait_values);
        device.queue_submit(
            self.queue,
            &[vk::SubmitInfo::default()
                .wait_semaphores(&wait_semaphores)
                .wait_dst_stage_mask(&wait_dst_stage)
                .command_buffers(slice::from_ref(&frame.command_buffer))
                .signal_semaphores(slice::from_ref(&target.render_finished))
                .push_next(&mut timeline_submit)],
            frame.in_flight,
        )?;
        Ok(())
    }
}
