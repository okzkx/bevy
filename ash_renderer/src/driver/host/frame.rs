//! ash 帧循环：一次 update = 一帧——处理 resize → 等帧槽位空出 → acquire →
//! 组装 DrawList → 帧槽 UBO 与环写入 → compute 段 dispatch 与诊断回读 →
//! overlay 绘制 → 录制并提交 → present。
//!
//! 资源内聚在各模块：本模块只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
//! 系统注册与禁渲染补位在 [`super`]（AshHostPlugin），初始化与拆除链在 [`super::init`]。
//!
//! 调度：`Last`，与采集同帧闭环——Update 变更 → PostUpdate 传播 + `scene::ledger`
//! 账本对账 → Last 消费账本：DrawList 槽位枚举与环写入（[`super::drawlist`]）→ 录制
//! 提交。上传票据在 GPU 侧等待，CPU 不阻塞。
//!
//! 帧槽：`frames.current_index()` 只在系统末尾的 `advance` 处推进，draw_frame 在
//! `wait_for_slot` 后取一次为 `slot`，本帧全部帧槽环/UBO/描述符集按它定位。
//!
//! 错误：错误点一行 `or_warn`/`or_fatal` 后 `?` 冒泡进统一错误通道，本系统只管控制流。
//! 让路语义也走 `?`（Warn 级）：最小化整帧让路、swapchain 过时本帧作废。present 真失败
//! 保持显式分支：先暂存，`advance` 照常跑保持帧槽账本一致后再冒泡。
//!
//! 坑：acquire 成功之后的 Vulkan 真失败必须致命退出——同步状态已无法恢复，warn 后
//! 继续会等一个永远无人 signal 的 fence，直接死锁。

use bevy::{
    ecs::system::NonSendMarker,
    prelude::*,
};

use ash_macros::system;

use crate::{
    common::syntax::fatal,
    scene::InstanceLedger,
    render::{ClusterFrameDraw, RenderFeatures, HizFrameDraw, PostChainDraw, SceneResources, UiDraw, UploadWait},
};

use super::drawlist::{self, DrawabilitySources, ParityDirty};

mod cluster;
mod culling;
mod gaps;
mod indirect;
mod input;
mod lighting;
mod lod;
mod overlay;
mod post;
mod resize_gate;
mod ring;
mod scene;
mod shadow;
mod stopwatch;
mod transparent;
mod frame_logs;

use self::input::{FrameInput, PassFrame};
use self::cluster::create_cluster_build_draw;
use self::culling::{create_frustum_cull_draw, create_occlusion_draw};
use self::gaps::sample_gaps;
use self::indirect::create_indirect_draw;
use self::lighting::FrameLight;
use self::lod::create_lod_draw;
use self::overlay::create_overlay_draw;
use self::post::{bloom_draw, smaa_draws};
use self::resize_gate::rebuild_size_dependent;
use self::scene::{scene_world_aabb, FrameView};
use self::shadow::shadow_frame_draw;
use self::stopwatch::FrameStopwatch;
use self::transparent::transparent_draw;

/// ash 帧循环系统（注册与排序在 [`super`]；流程与错误分流见模块文档）。
#[system]
pub(crate) fn draw_frame(
    mut input: FrameInput,
    _main_thread: NonSendMarker,
) -> Result<(), BevyError> {
    let FrameInput {
        gpu,
        timing,
        scene,
        frustum_cull,
        indirect,
        occlusion,
        lod,
        shadow,
        post,
        cluster,
        transparent,
        gap_snapshot,
        overlay,
        gate,
        logs,
        occlusion_gate,
    } = &mut input;

    // —— 帧首：最小化让路 + 挂起重建（尺寸相关资源随新 extent 重建，幂等；pending
    // 全部重建成功才清——失败帧让路或退出，下个帧首原样重试）。
    gate.yield_minimized(&mut gpu.resized)?;
    gate.rebuild_if_pending(gpu.swapchain.pending_rebuild(), || rebuild_size_dependent(gpu, &mut occlusion.hiz, post, cluster))?;

    // —— 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在提交前一刻）。
    let mut watch = FrameStopwatch::start(&mut timing.perf);
    gpu.frames.wait_for_slot()?;
    let slot = gpu.frames.current_index();
    watch.wait_slot_done(&mut timing.timer, slot)?;

    // —— acquire：OUT_OF_DATE = 这个 swapchain 已不可用（最小化/恢复/独占模式切换等），
    // 必须立即重建，等不得下一帧
    // —— acquire：OUT_OF_DATE/次优在 Swapchain::acquire_index 内消化（置位 + 让路），
    // 重建交给下个帧首（幂等）；次优照常画完本帧。
    let index = gpu.swapchain.acquire_index(gpu.frames.current().image_available)?;
    watch.acquire_done();
    let extent = gpu.swapchain.extent;

    // —— 组装 DrawList（增量）：实例槽缺/mesh 未驻留/材质缺席的行暂缓，就绪后自愈。
    let sources = DrawabilitySources::new(&scene.pool, &scene.std_materials, &scene.lod_groups, scene.residency_settings.floor_anchor);
    drawlist::assemble_update(&mut scene.ledger, sources);
    watch.assemble_done();
    scene.verify.verify_assembly(&scene.ledger, sources);
    // 奇偶脏集先于视图取走，此后 assembly 视图对账本共享借用
    let dirty = ParityDirty::take(&mut scene.ledger, slot);
    let ledger: &InstanceLedger = &scene.ledger;
    let assembly = ledger.assembly_view();
    let pending_rows = ledger.len() - assembly.slots.len();
    logs.log_assembly_status(pending_rows, ledger.len(), assembly.alpha_override);

    // —— 帧槽 UBO 与环写入：复用安全靠 wait_for_slot 的 fence——写发生在本槽上一轮
    // 使用完成之后（帧槽轮转只是定位）。
    let render_mode = *scene.render_mode;
    let view = FrameView::from_camera(&scene.cameras, logs);
    let light = FrameLight::gather(
        &scene.lights,
        &view,
        scene.ambient.as_deref(),
        render_mode,
        ledger.is_empty(),
        logs,
    );
    gpu.frame_uniform_table
        .write_frame_ubo(slot, &view, &light, render_mode)?;
    let scene_aabb = scene_world_aabb(ledger, &scene.pool);
    shadow.write_ubo(slot, &view, &light, scene_aabb)?;
    post.write_slot_ubos(slot)?;
    cluster.write_slot(slot, &view, extent)?;
    transparent.clear_slot_count(slot)?;

    // 四环写入：模型增量 + 材质全量 + 在场全量 + LOD 表增量，都落本帧槽的环
    // （wait_for_slot 之后，fence 证据同本帧 UBO）。
    scene.instance_tables.write_model_ring(ledger, &dirty.model, slot)?;
    scene
        .instance_tables
        .write_material_ring(sources.std_materials, &scene.image_cache, assembly.materials, slot)?;
    scene.instance_tables.write_presence_ring(assembly.slots, slot)?;
    scene.instance_tables.write_lod_table_ring(
        ledger,
        sources.pool,
        sources.lod_groups,
        &dirty.lod,
        slot,
    )?;
    watch.rings_done();

    // —— compute 段：各步骤按开关决定 dispatch 或 host 填充（A/B 对照），诊断读数在
    // 写入路径之后取，都在 wait_for_slot 的 fence 保护内。诊断只进计量，任何系统不得
    // 据此决定画什么。
    let pass_frame = PassFrame {
        slot,
        bound: scene.instance_tables.used_object_slots(),
        view_proj: view.view_proj,
        screen: [extent.width as f32, extent.height as f32],
        live: assembly.slots,
    };
    let instances = &mut scene.instance_tables;

    // 视锥剔除：GPU 一个 dispatch 兼产两份标记——主视图 marks + 四级阴影视锥
    // shadow_marks；主视图标记受主开关控制（关 = 恒写全可见），阴影标记不受影响。
    let (frustum_cull_draw, visible_among_live) = create_frustum_cull_draw(
        &pass_frame,
        frustum_cull,
        shadow.settings.enabled,
        instances,
        &mut logs.cull_first_dispatch,
    )?;

    // 遮挡剔除：读两帧前的 Hi-Z 金字塔做遮挡测试；相机跳变/金字塔未就绪等判据
    // 任一命中则跳过 dispatch（b3 保持视锥结果、b6 host 填全可见）。跳变判定推入
    // 本帧机位、对照两帧前——金字塔拍摄时的机位。
    let camera_jumped = occlusion_gate.push(view.camera.map(|c| (c.pos, c.rot)));
    let occl_dispatch = create_occlusion_draw(
        &pass_frame,
        occlusion,
        frustum_cull.settings.enabled,
        camera_jumped,
        instances,
        &mut logs.occlusion_first_dispatch,
    )?;

    // LOD 选档：开关决定 dispatch 或 host 档位环填最细档（选档每帧现算，无 fallback）。
    let lod_dispatch = create_lod_draw(&pass_frame, lod, instances, &mut logs.lod_first_dispatch)?;
    // 缺口快照：读本槽缺口标记环（视锥剔除关闭帧 = host 全可见口径）。
    sample_gaps(&pass_frame, gap_snapshot, instances, frustum_cull_draw.is_none())?;

    // indirect 命令组装：在场行数 = GPU 命令条数；空场 = None（场景绘制整段跳过）。
    let indirect_count = assembly.slots.len() as u32;
    let indirect_dispatch = create_indirect_draw(
        indirect,
        indirect_count,
        visible_among_live,
        ledger.len(),
        pending_rows,
        &mut logs.indirect_first_submit,
    )?;

    // —— overlay 绘制：图集整传（可能多一张 transfer 票据，本帧 upload_wait 快照必须在此后取，
    // 见 UploadWait::snapshot）→ 镶嵌 → 顶点环写入。UI 数据缺席 = None（record_frame 的
    // UI 段整段跳过）。
    let ui = create_overlay_draw(&gpu.ctx, &mut gpu.tables, &mut gpu.uploader, overlay, slot, extent)?;

    // —— 录制 + 提交。上传票据在 GPU 侧等待（ALL_COMMANDS），CPU 不阻塞。
    // 坑：image_available 已被置位、image 已到手，从这里起任何失败都只能致命退出，
    // 不能 warn 后继续（理由见模块文档）。
    // 本帧呈现目标与 render_finished 同号（按 image 分配），录制提交与 present 共用
    let target = gpu.swapchain.target(index);
    let instances = &scene.instance_tables;
    let transparent_frame =
        transparent_draw(transparent, slot, indirect_count)?;
    // shadow：命令组装（读 shadow 标记 + LOD 档位环，逐在场行 × 每 cascade，无遮挡参与、
    // LOD 再粗一档）与 depth pass 同门；空场/关闭整段跳过，set 与深度图屏障恒录。
    let shadow_frame = shadow_frame_draw(shadow, instances, &scene.pool, slot, indirect_count)?;
    let (smaa_edge_blend, smaa_nb) = smaa_draws(post, slot);

    // —— ④⑤ 场景资源与规格（feature 各束的共用底座，②⑤⑧ 复绑）
    let res = SceneResources {
        pipeline: post.gbuffer.pipeline.rec(),
        bindless_set: gpu.tables.descriptor_set(),
        frame_set: gpu.frame_uniform_table.frame_set(slot),
        instance_set: instances.frame_set(slot),
        vertex_buffer: scene.pool.vertex_buffer(),
        index_buffer: scene.pool.index_buffer(),
        // 命令环句柄在 count = 0 时不被读（record_frame 的场景绘制整段跳过）
        indirect_count,
        indirect_buffer: instances.indirect_buffer(slot),
        // SceneResources 借 GBufferTargets 引用（record_frame 消费附件视图与采样集）
        gbuffer: &post.gbuffer.targets,
        composite: post.composite.rec(),
    };
    let feature = RenderFeatures {
        // —— ① 帧首
        timer: timing.timer.enabled.then_some(&*timing.timer),
        // —— ② compute 前置
        frustum_cull: frustum_cull_draw,
        hiz: HizFrameDraw { occlusion: occl_dispatch, pyramid: &occlusion.hiz },
        lod: lod_dispatch,
        indirect: indirect_dispatch,
        shadow: shadow_frame,
        cluster: ClusterFrameDraw {
            build: create_cluster_build_draw(cluster, indirect_count),
            set: cluster.clusters.frame_set(slot),
        },
        transparent: transparent_frame,
        // —— ⑦⑧ 后链 + UI
        post: PostChainDraw {
            tonemap: post.tonemap.rec(),
            bloom_params_set: post.bloom.params_set(slot),
            bloom: bloom_draw(post, slot),
            smaa_edge_blend,
            smaa_nb,
        },
        // 图集整传 → 镶嵌 → 顶点环写入后才有 UI 产物；无 UI 跳过
        ui: ui.as_ref().map(|(paint, buffers)| UiDraw {
            overlay: overlay.pipeline.rec(),
            paint,
            buffers: *buffers,
        }),
        // —— ⑨ 提交
        upload_wait: UploadWait::snapshot(&gpu.uploader),
    };
    gpu.frames
        .record_frame(
            &target,
            &feature,
            &res,
        )?;
    watch.record_done();

    // —— present：OUT_OF_DATE 已在 present_digest 内标过时（下个帧首重建）；真失败
    // 先暂存——提交已发生，advance 必须照常推进保持帧槽账本一致，收尾后在系统末尾冒泡。
    let present_failure = gpu.swapchain.present_digest(gpu.ctx.queue, target.render_finished, index);
    watch.present_done();

    // —— 帧样本入账：只进计量。让路帧（最小化/acquire 过时）不经过此处。
    watch.push(&mut timing.perf, indirect_count, visible_among_live);

    occlusion.hiz.note_reduced();
    gpu.frames.advance();
    match present_failure {
        Some(e) => Err(fatal("present 失败，渲染链无法继续", e)),
        None => Ok(()),
    }
}

