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
    camera::{Camera, Projection},
    ecs::system::{NonSendMarker, SystemParam},
    light::{AmbientLight, DirectionalLight, GlobalAmbientLight},
    math::{Mat4, Quat},
    prelude::*,
    window::WindowResized,
};

use ash::vk;

use ash_macros::system;

use crate::{
    common::bytes::put_f32s,
    common::error::VulkanError,
    common::syntax::{fatal, OnceLog, OptionTierExt, ResultTierExt},
    overlay::{paint_overlay, OverlayLogState, UiDrawData},
    scene::{
        DrawListVerify, GapSnapshot, InstanceLedger, LodGroups, ResidencySettings,
    },
    render::{
        cascade_matrices, conservative_world_aabb, frustum_planes, pack_bloom_params,
        pack_cluster_uniforms, pack_frame_uniforms, pack_shadow_uniforms, BindlessTextureTable, BloomFrameDraw, BloomParamsData, BloomPipelines, BloomSettings,
        BloomTargets, ClusterBuildPass, CompositePipeline, Context, CullDiagnostics, CullDraw,
        CullPass, CullSettings, FrameDraw, FramePool, FrameUniformTable,
        FrameUniformsData, GBufferPipeline, GBufferTargets, GpuBuffer, GpuTimer, HizPyramid,
        ImageCache, IndirectDraw, IndirectPass, IndirectStats, InstanceTables, LightClusters,
        LightTable, LodDiagnostics, LodDraw, LodPass, LodSettings, MeshPool, OcclusionDiagnostics,
        OcclusionDraw, OcclusionFallback, OcclusionPass, OcclusionSettings, OverlayPipeline,
        PerfLedger, PresentTonemapPipeline, RenderMode, ShadowBuildPass, ShadowCascadeInput,
        ShadowCascades, ShadowDiagnostics, ShadowPipeline, ShadowSettings, ShadowTargets,
        ShadowUniformsData, ShadowWriteDraw, SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines,
        SmaaSettings, SmaaTargets, Swapchain, TransparentFrame, TransparentPasses,
        TransparentPipeline, TransparentTargets, Uploader, GAP_MARK_SIZE, INDIRECT_COMMAND_SIZE,
        LOD_LEVEL_SIZE, MARK_SIZE, MAX_FRAMES_IN_FLIGHT, MAX_LOD_LEVELS,
        PLACEHOLDER_DIR_TO_LIGHT, SHADOW_CASCADES, SHADOW_MAP_SIZE,
    },
};

use super::drawlist::{self, DrawabilitySources, ParityDirty};

mod resize_gate;
mod stopwatch;
mod frame_logs;

use self::resize_gate::ResizeGate;
use self::stopwatch::FrameStopwatch;
use self::frame_logs::FrameLogs;

/// 遮挡 fallback 的相机历史（`draw_frame` 的 `Local`，跨帧保持）：跳变判定用
/// "当前 vs 两帧前"——遮挡读的金字塔正是两帧前的深度。阈值见 occlusion 模块文档，
/// 连续缓慢环绕不触发。
#[derive(Default)]
struct OcclusionGate {
    prev: Option<(Vec3, Quat)>,
    prev_prev: Option<(Vec3, Quat)>,
}

impl OcclusionGate {
    /// 推入本帧机位并返回"相对两帧前是否跳变"（历史不足 = 未就绪 = 视同跳变）。
    fn push(&mut self, now: Option<(Vec3, Quat)>) -> bool {
        let jump = match (self.prev_prev, now) {
            (Some((p0, r0)), Some((p1, r1))) => {
                (p1 - p0).length() > 0.5 // 世界单位
                    || Quat::angle_between(r0, r1) > 15f32.to_radians()
            }
            _ => true, // 启动/相机缺席帧
        };
        self.prev_prev = self.prev.take();
        self.prev = now;
        jump
    }
}

/// 相机查询：投影 + 位姿 + 可选的每相机 `AmbientLight` 覆盖（挂了则压过全局资源）。
type CameraQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static Projection,
        &'static GlobalTransform,
        Option<&'static AmbientLight>,
    ),
    With<Camera>,
>;

/// 方向光查询（取第一盏，方向 = 实体 `back()`）。
type LightQuery<'w, 's> = Query<'w, 's, (&'static GlobalTransform, &'static DirectionalLight)>;

/// draw_frame 的参数束：按特性分子束，系统签名只剩本束 + 主线程标记。
#[derive(SystemParam)]
pub(crate) struct FrameInput<'w, 's> {
    core: FrameCore<'w, 's>,
    scene: SceneInput<'w, 's>,
    cull: CullInput<'w>,
    indirect: IndirectInput<'w>,
    occlusion: OcclusionInput<'w>,
    lod: LodInput<'w>,
    shadow: ShadowInput<'w>,
    post: PostInput<'w>,
    cluster: ClusterInput<'w>,
    transparent: TransparentInput<'w>,
    /// GPU 缺口快照：帧起在 fence 保护内读缺口标记环的结果，驻留管理器 tick（Last，帧
    /// 提交后）与 BRP census 经此消费，不直读环。
    gap_snapshot: ResMut<'w, GapSnapshot>,
    /// overlay 绘制半边资源束（None = overlay 未装，UI 段整段跳过）。
    overlay: Option<UiDrawData<'w>>,
    gate: Local<'s, ResizeGate>,
    logs: Local<'s, FrameLogs>,
    occlusion_gate: Local<'s, OcclusionGate>,
    paint_log: Local<'s, OverlayLogState>,
}

/// 帧骨架：设备/交换链/帧槽/常驻表 + 场景与 overlay 管线 + 提交侧票号/计时/计量。
#[derive(SystemParam)]
pub(crate) struct FrameCore<'w, 's> {
    ctx: Res<'w, Context>,
    swapchain: ResMut<'w, Swapchain>,
    frames: ResMut<'w, FramePool>,
    tables: ResMut<'w, BindlessTextureTable>,
    frame_uniform_table: ResMut<'w, FrameUniformTable>,
    resized: MessageReader<'w, 's, WindowResized>,
    gbuffer_pipeline: Res<'w, GBufferPipeline>,
    /// overlay 管线：UI 半边绑定用（init 链与场景管线同批创建）。
    overlay_pipeline: Res<'w, OverlayPipeline>,
    /// 上传器：普通帧只读票号，overlay 在场时图集整传要走它提交批次（独占写点）。
    uploader: ResMut<'w, Uploader>,
    /// GPU 帧计时器（停用时 enabled=false，GPU 区间读数缺席）。
    timer: ResMut<'w, GpuTimer>,
    /// 帧样本账（每帧 push 一份 CPU 分段 + GPU 区间 + 规模；只进计量）。
    perf: ResMut<'w, PerfLedger>,
}

/// 场景侧输入：账本、资产容器与驻留、GPU 常驻绘制数据、相机与灯光。
#[derive(SystemParam)]
pub(crate) struct SceneInput<'w, 's> {
    /// 稳定槽账本（跨帧增量维护；模型环待写集的独占消费点在本系统）。
    ledger: ResMut<'w, InstanceLedger>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    pool: Res<'w, MeshPool>,
    image_cache: Res<'w, ImageCache>,
    /// GPU 常驻绘制数据（对象表/各帧槽环）：本帧环写入与标记环读写的写点。
    instance_tables: ResMut<'w, InstanceTables>,
    /// LOD 组登记处（只读——可绘锚解析与 LOD 表环写入的变体柄来源）。
    lod_groups: Res<'w, LodGroups>,
    /// 驻留管理器设置（DrawList 可绘锚解析读 floor_anchor 开关）。
    residency_settings: Res<'w, ResidencySettings>,
    /// 组装对照账面（BRP 开关；关闭时零成本）。
    verify: ResMut<'w, DrawListVerify>,
    cameras: CameraQuery<'w, 's>,
    lights: LightQuery<'w, 's>,
    ambient: Option<Res<'w, GlobalAmbientLight>>,
    /// 着色模式（AshHostPlugin 恒插入；overlay 单选钮与 BRP 写）。
    render_mode: Res<'w, RenderMode>,
}

/// 视锥剔除：compute 管线 + 开关（egui/BRP 写，AshHostPlugin 恒插入）+ 诊断账面。
#[derive(SystemParam)]
pub(crate) struct CullInput<'w> {
    pass: Res<'w, CullPass>,
    settings: Res<'w, CullSettings>,
    diag: ResMut<'w, CullDiagnostics>,
}

/// indirect 命令组装：compute 管线 + 诊断账面（只进计量）。
#[derive(SystemParam)]
pub(crate) struct IndirectInput<'w> {
    pass: Res<'w, IndirectPass>,
    stats: ResMut<'w, IndirectStats>,
}

/// Hi-Z 遮挡：测试管线 + 金字塔（归约链录制句柄 + warmup 走账）+ 开关 + 诊断账面。
#[derive(SystemParam)]
pub(crate) struct OcclusionInput<'w> {
    pass: Res<'w, OcclusionPass>,
    hiz: ResMut<'w, HizPyramid>,
    settings: Res<'w, OcclusionSettings>,
    diag: ResMut<'w, OcclusionDiagnostics>,
}

/// GPU LOD 选档：compute 管线 + 开关与判据参数 + 诊断账面。
#[derive(SystemParam)]
pub(crate) struct LodInput<'w> {
    pass: Res<'w, LodPass>,
    settings: Res<'w, LodSettings>,
    diag: ResMut<'w, LodDiagnostics>,
}

/// 级联阴影：资源族（级联 UBO/标记/命令环 + 深度图 + 描述符）+ depth-only 绘制
/// 管线 + 命令组装 compute + 开关 + 诊断账面（BRP shadow_stats 读数口）。
#[derive(SystemParam)]
pub(crate) struct ShadowInput<'w> {
    targets: ResMut<'w, ShadowTargets>,
    pipeline: Res<'w, ShadowPipeline>,
    build_pass: Res<'w, ShadowBuildPass>,
    settings: Res<'w, ShadowSettings>,
    diag: ResMut<'w, ShadowDiagnostics>,
}

/// 延迟光照与后处理：G-buffer 附件 + HDR 中间目标（随 resize 重建）、
/// 合成管线（G-buffer 光照 → HDR）、present tonemap（HDR + Bloom → ACES →
/// LDR）、Bloom 链与 SMAA 的资源/管线/开关。
#[derive(SystemParam)]
pub(crate) struct PostInput<'w> {
    gbuffer: ResMut<'w, GBufferTargets>,
    composite: Res<'w, CompositePipeline>,
    tonemap: Res<'w, PresentTonemapPipeline>,
    bloom: ResMut<'w, BloomTargets>,
    bloom_pipelines: Res<'w, BloomPipelines>,
    bloom_settings: Res<'w, BloomSettings>,
    smaa: ResMut<'w, SmaaTargets>,
    smaa_pipelines: Res<'w, SmaaPipelines>,
    smaa_settings: Res<'w, SmaaSettings>,
}

/// 光源分簇：cluster 资源、cluster_build 管线和光源常驻表。合成与透明前向共用。
#[derive(SystemParam)]
pub(crate) struct ClusterInput<'w> {
    clusters: ResMut<'w, LightClusters>,
    build_pass: Res<'w, ClusterBuildPass>,
    lights: ResMut<'w, LightTable>,
}

/// 透明层：透明命令链资源、三条 compute 管线和透明前向管线。
#[derive(SystemParam)]
pub(crate) struct TransparentInput<'w> {
    targets: ResMut<'w, TransparentTargets>,
    passes: Res<'w, TransparentPasses>,
    pipeline: Res<'w, TransparentPipeline>,
}

/// ash 帧循环系统（注册与排序在 [`super`]；流程与错误分流见模块文档）。
#[system]
pub(crate) fn draw_frame(
    mut input: FrameInput,
    _main_thread: NonSendMarker,
) -> Result<(), BevyError> {
    let FrameInput {
        core,
        scene,
        cull,
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
        paint_log,
    } = &mut input;
    gate.yield_minimized(&mut core.resized)?;
    // pending 全部重建成功才清：失败帧让路或退出，下个帧首原样重试
    gate.rebuild_if_pending(core.swapchain.pending_rebuild(), || rebuild_size_dependent(core, &mut occlusion.hiz, post, cluster))?;

    // —— 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在提交前一刻）。
    let mut watch = FrameStopwatch::start(&mut core.perf);
    core.frames.wait_for_slot()?;
    let slot = core.frames.current_index();
    watch.wait_slot_done(&mut core.timer, slot)?;

    // —— acquire：OUT_OF_DATE = 这个 swapchain 已不可用（最小化/恢复/独占模式切换等），
    // 必须立即重建，等不得下一帧
    // —— acquire：OUT_OF_DATE/次优在 Swapchain::acquire_index 内消化（置位 + 让路），
    // 重建交给下个帧首（幂等）；次优照常画完本帧。
    let index = core.swapchain.acquire_index(core.frames.current().image_available)?;
    watch.acquire_done();

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
    core.frame_uniform_table
        .write_frame_ubo(slot, &view, &light, render_mode)?;
    let scene_aabb = scene_world_aabb(ledger, &scene.pool);
    shadow.write_ubo(slot, &view, &light, scene_aabb)?;
    post.write_slot_ubos(slot)?;
    cluster.write_slot(slot, &view, core.swapchain.extent)?;
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

    // —— compute 段：各半边按开关决定 dispatch 或 host 填充（A/B 对照），诊断读数在
    // 写入路径之后取，都在 wait_for_slot 的 fence 保护内。诊断只进计量，任何系统不得
    // 据此决定画什么。
    let extent = core.swapchain.extent;
    let pass_frame = PassFrame {
        slot,
        bound: scene.instance_tables.used_object_slots(),
        view_proj: view.view_proj,
        screen: [extent.width as f32, extent.height as f32],
        live: assembly.slots,
    };
    let instances = &mut scene.instance_tables;
    let (cull_dispatch, visible_among_live) = cull_half(
        &pass_frame,
        cull,
        shadow.settings.enabled,
        instances,
        &mut logs.cull_first_dispatch,
    )?;
    shadow_marks_half(&pass_frame, shadow)?;
    let camera_jumped = occlusion_gate.push(view.camera.map(|c| (c.pos, c.rot)));
    let occl_dispatch = occlusion_half(
        &pass_frame,
        occlusion,
        cull.settings.enabled,
        camera_jumped,
        instances,
        &mut logs.occlusion_first_dispatch,
    )?;
    let lod_dispatch = lod_half(&pass_frame, lod, instances, &mut logs.lod_first_dispatch)?;
    sample_gaps(&pass_frame, gap_snapshot, instances, cull_dispatch.is_none())?;
    let indirect_count = assembly.slots.len() as u32;
    let indirect_dispatch = indirect_half(
        indirect,
        indirect_count,
        visible_among_live,
        ledger.len(),
        pending_rows,
        &mut logs.indirect_first_submit,
    )?;
    // shadow 命令组装半边：读 shadow 标记 + LOD 档位环，逐在场行 × 每 cascade
    // 组装命令（无遮挡参与、LOD 再粗一档）。空场/关闭 = None（shadow pass 整段跳过）。
    let shadow_build_dispatch = (shadow.settings.enabled && indirect_count > 0)
        .then(|| shadow.build_pass.draw(indirect_count));

    // —— overlay 绘制半边：图集整传（可能多一张 transfer 票据，所以本帧 wait_ticket 必须
    // 在此后取）→ 镶嵌 → 顶点环写入。overlay 未装 = 整段跳过。
    let ui = match overlay.take() {
        Some(mut ui_data) => {
            let ui_buffers = ui_data.ring.buffers(slot);
            let paint = paint_overlay(
                &core.ctx,
                &ui_data.egui,
                &mut ui_data.frame,
                &ui_data.mirror,
                &mut ui_data.gpu,
                &mut ui_data.ring,
                &mut core.tables,
                &mut core.uploader,
                slot,
                extent,
                paint_log,
            )?;
            paint.map(|p| (p, ui_buffers))
        }
        None => None,
    };

    // —— 录制 + 提交。上传票据在 GPU 侧等待（ALL_COMMANDS），CPU 不阻塞。
    // 坑：image_available 已被置位、image 已到手，从这里起任何失败都只能致命退出，
    // 不能 warn 后继续（理由见模块文档）。
    let image = core.swapchain.images[index as usize];
    let target_view = core.swapchain.views[index as usize];
    let instances = &scene.instance_tables;
    let transparent_frame =
        transparent_draw(transparent, instances, slot, indirect_count)?;
    let shadow_write = shadow_write_draw(shadow, instances, &scene.pool, slot, indirect_count)?;
    let (smaa_edge_blend, smaa_nb) = smaa_draws(post, slot);
    let frame_draw = FrameDraw {
        pipeline: core.gbuffer_pipeline.rec(),
        set0: core.tables.descriptor_set(),
        set1: core.frame_uniform_table.frame_set(slot),
        set2: instances.frame_set2(slot),
        vertex_buffer: scene.pool.vertex_buffer(),
        index_buffer: scene.pool.index_buffer(),
        // 命令环句柄在 count = 0 时不被读（record_frame 的场景绘制整段跳过）
        indirect_count,
        indirect_buffer: instances.indirect_buffer(slot),
        // FrameDraw 借 GBufferTargets 引用（record_frame 消费附件视图与采样集）
        gbuffer: &post.gbuffer,
        composite: post.composite.rec(),
        cluster_set: cluster.clusters.frame_set(slot),
        cluster_build: (indirect_count > 0)
            .then(|| cluster.build_pass.draw(cluster.clusters.grid())),
        tonemap: post.tonemap.rec(),
        bloom_params_set: post.bloom.params_set(slot),
        bloom: bloom_draw(post, slot),
        smaa_edge_blend,
        smaa_nb,
        overlay: core.overlay_pipeline.rec(),
        ui: ui.as_ref().map(|(paint, _)| paint),
        // ui 为 None 时这两个句柄不被读（record_frame 的 UI 段整段跳过）
        ui_buffers: ui
            .as_ref()
            .map(|(_, buffers)| *buffers)
            .unwrap_or((vk::Buffer::null(), vk::Buffer::null())),
        wait_ticket: core.uploader.last_issued_ticket(),
        ticket_semaphore: core.uploader.ticket_semaphore(),
        clear: CLEAR_COLOR,
        cull: cull_dispatch,
        shadow_build: shadow_build_dispatch,
        shadow: shadow_write,
        shadow_set: shadow.targets.frame_set(slot),
        shadow_images: std::array::from_fn(|c| shadow.targets.map_image(c)),
        indirect: indirect_dispatch,
        occlusion: occl_dispatch,
        lod: lod_dispatch,
        transparent: transparent_frame,
        // 归约链恒录，warmup 走账与录制一一对应（录制后 note_reduced）
        hiz: Some(&*occlusion.hiz),
        timer: core.timer.enabled.then_some(&*core.timer),
    };
    core.frames
        .record_frame(
            &core.ctx,
            core.swapchain.render_finished[index as usize],
            image,
            target_view,
            extent,
            &frame_draw,
        )?;
    watch.record_done();

    // —— present：等待的信号量按 acquire 的 image index 取（不按帧槽轮转）。
    // OUT_OF_DATE 只标 pending，交给下个帧首重建；其余真失败先暂存——提交已发生，
    // advance 必须照常推进保持帧槽账本一致，收尾后在系统末尾冒泡。
    let present_failure = match core.swapchain.present(
        core.ctx.queue,
        core.swapchain.render_finished[index as usize],
        index,
    ) {
        Ok(_) => None,
        Err(VulkanError::SwapchainOutOfDate) => {
            core.swapchain.mark_out_of_date();
            gate.request_rebuild();
            None
        }
        Err(e) => Some(e),
    };
    watch.present_done();

    // —— 帧样本入账：只进计量。让路帧（最小化/acquire 过时）不经过此处。
    watch.push(&mut core.perf, indirect_count, visible_among_live);

    occlusion.hiz.note_reduced();
    core.frames.advance();
    match present_failure {
        Some(e) => Err(fatal("present 失败，渲染链无法继续", e)),
        None => Ok(()),
    }
}

/// 尺寸变化 → 帧首按需重建，时机在 acquire 之前。rebuild 幂等（尺寸没变就空手而归），
/// 拖拽中每步一建、帧循环不断流。其余尺寸相关资源随其后按新 extent 重建（同样幂等；
/// 各自带 device_wait_idle，不会引用到在飞旧资源）。swapchain 重建失败让路下帧重试，
/// 其余失败致命退出。
fn rebuild_size_dependent(
    core: &mut FrameCore,
    hiz: &mut HizPyramid,
    post: &mut PostInput,
    cluster: &mut ClusterInput,
) -> Result<(), BevyError> {
    let ctx: &Context = &core.ctx;
    core.swapchain
        .rebuild(ctx)
        .or_warn("resize 后重建失败，下帧重试")?;
    let extent = core.swapchain.extent;
    core.frames
        .rebuild_depth(ctx, extent)
        .or_fatal("深度附件重建失败，渲染链无法继续")?;
    // Hi-Z 金字塔随深度附件同批重建（warmup 重置，遮挡测试让路到金字塔重新就绪）
    hiz.rebuild(ctx, extent, &per_slot(|s| core.frames.depth_image(s)))
        .or_fatal("Hi-Z 金字塔重建失败，渲染链无法继续")?;
    post.bloom
        .rebuild(ctx, extent)
        .or_fatal("Bloom 链重建失败，渲染链无法继续")?;
    // 依赖上面已重建的深度视图（G-buffer 采样集 b3）
    post.gbuffer
        .rebuild(ctx, extent, &per_slot(|s| core.frames.depth_view(s)))
        .or_fatal("G-buffer 附件重建失败，渲染链无法继续")?;
    // 光源常驻表与尺寸无关不重建，只把它的 buffer 重新绑进新描述符集
    cluster
        .clusters
        .rebuild(ctx, extent, &per_slot(|s| cluster.lights.ring_buffer(s)))
        .or_fatal("光源分簇 cluster 表重建失败，渲染链无法继续")?;
    // tonemap/edge/blend 的 renderArea 与附件尺寸必须同帧对齐
    post.smaa
        .rebuild(ctx, extent)
        .or_fatal("SMAA 三目标重建失败，渲染链无法继续")
}

/// 逐帧槽收集（重建入参：深度 image/view、光源环 buffer）。
fn per_slot<T>(f: impl FnMut(usize) -> T) -> Vec<T> {
    (0..MAX_FRAMES_IN_FLIGHT).map(f).collect()
}

/// 非透视投影的 near 兜底：级联的 near/d 视深换算只对透视投影成立，正交下
/// 级联拟合不保证正确，此值只保证换算有定义。
const NON_PERSPECTIVE_NEAR: f32 = 100.0;

/// 相机位姿（相机在场帧才有）：遮挡跳变判定、级联拟合与光照 V 向量的输入。
#[derive(Clone, Copy)]
struct CameraPose {
    pos: Vec3,
    rot: Quat,
    /// 投影 near（级联 z_ndc↔视深换算）。
    near: f32,
}

impl CameraPose {
    fn forward(&self) -> Vec3 {
        self.rot * Vec3::NEG_Z
    }
}

/// 本帧视图：相机矩阵 + 位姿 + 每相机环境光覆盖。相机缺席是场景装配问题，
/// 报一次后 identity 兜底、位姿 None（画面不可读但不崩帧循环）。矩阵乘序/
/// 列主序与 WGSL 端同一约定。
struct FrameView<'a> {
    view_proj: Mat4,
    /// clip→world 反投影（深度重建与级联角点反推；与 view_proj 同帧同源）。
    inv_view_proj: Mat4,
    camera: Option<CameraPose>,
    /// 相机组件 `AmbientLight`（若挂）压过全局 `GlobalAmbientLight` 资源。
    ambient_override: Option<&'a AmbientLight>,
}

impl<'a> FrameView<'a> {
    fn from_camera(cameras: &'a CameraQuery, logs: &mut FrameLogs) -> Self {
        match cameras.single() {
            Ok((projection, global, ambient_override)) => {
                let view_proj = projection.get_clip_from_view() * global.affine().inverse();
                let near = match projection {
                    Projection::Perspective(p) => p.near,
                    _ => NON_PERSPECTIVE_NEAR,
                };
                Self {
                    view_proj,
                    // glam 的 Mat4::inverse 通用逆，投影矩阵可逆性由投影构造保证
                    inv_view_proj: view_proj.inverse(),
                    camera: Some(CameraPose {
                        pos: global.translation(),
                        rot: global.rotation(),
                        near,
                    }),
                    ambient_override,
                }
            }
            Err(_) => {
                logs.warned_no_camera.log(|| {
                    warn!("相机缺席：view_proj 用 identity 兜底（画面不可读属预期，查相机装配）")
                });
                Self {
                    view_proj: Mat4::IDENTITY,
                    inv_view_proj: Mat4::IDENTITY,
                    camera: None,
                    ambient_override: None,
                }
            }
        }
    }

    /// 相机世界位置（w = 0；相机缺席帧 zeros 兜底）。
    fn camera_pos4(&self) -> [f32; 4] {
        self.camera.map(|c| c.pos.extend(0.0).to_array()).unwrap_or([0.0; 4])
    }
}

/// 本帧光照输入（帧 UBO 与级联拟合同源）。
struct FrameLight {
    /// 实体 back()（forward 取负，"N·L 就绪"）；无灯 = 合法方向占位。
    dir_to_light: [f32; 4],
    /// linear × illuminance；无灯 = 直射项归零。
    light_color: [f32; 4],
    /// linear × brightness；两种来源均无 = 环境项归零。
    ambient_color: [f32; 4],
}

impl FrameLight {
    /// 方向光取第一盏（多灯告警一次，多灯支持未实现）；环境光相机覆盖优先。
    fn gather(
        lights: &LightQuery,
        view: &FrameView,
        global_ambient: Option<&GlobalAmbientLight>,
        render_mode: RenderMode,
        ledger_empty: bool,
        logs: &mut FrameLogs,
    ) -> Self {
        let light_count = lights.iter().count();
        let (dir_to_light, light_color) = match lights.iter().next() {
            Some((light_tf, light)) => {
                let back = light_tf.back();
                let [r, g, b, a] = LinearRgba::from(light.color).to_f32_array();
                (
                    [back.x, back.y, back.z, 0.0],
                    [r * light.illuminance, g * light.illuminance, b * light.illuminance, a],
                )
            }
            None => (PLACEHOLDER_DIR_TO_LIGHT, [0.0; 4]),
        };
        if light_count != 1 && !ledger_empty {
            logs.warned_light_count.log(|| {
                warn!("方向光 {light_count} 盏（取第一盏/无灯直射项归零），多灯支持未实现")
            });
        }
        let ambient = view
            .ambient_override
            .map(|a| (a.color, a.brightness))
            .or(global_ambient.map(|g| (g.color, g.brightness)));
        let ambient_color = match ambient {
            Some((color, brightness)) => LinearRgba::from(color).to_f32_array().map(|c| c * brightness),
            None => {
                logs.warned_no_ambient.log(|| {
                    warn!("环境光缺席：GlobalAmbientLight 资源与相机 AmbientLight 均无，环境项归零")
                });
                [0.0; 4]
            }
        };
        if light_count > 0 {
            logs.lights_logged.log(|| info!(
                "灯光 UBO 汇总：方向光 {light_count} 盏（dir_to_light = back()，light = {}）；\
                 环境 = {}（来源 {}）；材质模式 = {}（ASH_RENDER_MODE）",
                fmt_vec4(light_color),
                fmt_vec4(ambient_color),
                if view.ambient_override.is_some() { "相机 AmbientLight" } else { "GlobalAmbientLight" },
                render_mode.label(),
            ));
        }
        Self { dir_to_light, light_color, ambient_color }
    }
}

/// 本帧槽主 UBO：相机矩阵 + 灯光 + 材质模式 + 相机世界位置（光照累积 V 向量）。
impl FrameUniformTable {
    /// impl 留在 driver 层而非 frame_uniform_table.rs：方法消费本模块私有的
    /// FrameView/FrameLight，放 render 侧会反向依赖（同四环写入的落位理由）。
    fn write_frame_ubo(
        &mut self,
        slot: usize,
        view: &FrameView,
        light: &FrameLight,
        render_mode: RenderMode,
    ) -> Result<(), BevyError> {
        self.ubo(slot)
            .write(
                0,
                &pack_frame_uniforms(&FrameUniformsData {
                    view_proj: view.view_proj,
                    inv_view_proj: view.inv_view_proj,
                    dir_to_light: light.dir_to_light,
                    ambient_color: light.ambient_color,
                    light_color: light.light_color,
                    mode: render_mode.as_u32(),
                    camera_pos: view.camera_pos4(),
                }),
            )
            .or_fatal("帧 UBO 写失败，渲染链无法继续")
    }
}

/// 场景世界 AABB 并集（级联 far 外扩与深度盒向太阳侧延伸的输入）。
fn scene_world_aabb(ledger: &InstanceLedger, pool: &MeshPool) -> Option<(Vec3, Vec3)> {
    let mut scene_aabb: Option<(Vec3, Vec3)> = None;
    for (_, row) in ledger.rows() {
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else { continue };
        let (wmin, wmax) =
            conservative_world_aabb(&row.model, mesh_slot.local_aabb.0, mesh_slot.local_aabb.1);
        let (wmin, wmax) = (Vec3::from_array(wmin), Vec3::from_array(wmax));
        scene_aabb = Some(match scene_aabb {
            None => (wmin, wmax),
            Some((mn, mx)) => (mn.min(wmin), mx.max(wmax)),
        });
    }
    scene_aabb
}

impl ShadowInput<'_> {
    /// shadow UBO：级联矩阵（log 分割拟合）+ 四×六平面（cull 内联标记与 shadow_stats
    /// CPU 参照同源）+ 分割带/相机基（composite 选级联）。相机缺席帧 = identity 矩阵。
    fn write_ubo(
        &mut self,
        slot: usize,
        view: &FrameView,
        light: &FrameLight,
        scene_aabb: Option<(Vec3, Vec3)>,
    ) -> Result<(), BevyError> {
        let fit = match view.camera {
            Some(cam) => cascade_matrices(&ShadowCascadeInput {
                inv_view_proj: view.inv_view_proj,
                camera_pos: cam.pos,
                camera_forward: cam.forward(),
                near: cam.near,
                dir_to_light: Vec3::from_slice(&light.dir_to_light[..3]),
                scene_aabb,
            }),
            None => ShadowCascades {
                view_proj: [Mat4::IDENTITY; SHADOW_CASCADES],
                splits: [f32::INFINITY; SHADOW_CASCADES],
                texel_world: [1.0; SHADOW_CASCADES],
                inv_span: [0.0; SHADOW_CASCADES],
            },
        };
        let cascade_planes = fit.view_proj.map(|m| frustum_planes(&m));
        let uniform = ShadowUniformsData {
            cascade_view_proj: fit.view_proj,
            planes: std::array::from_fn(|i| cascade_planes[i / 6][i % 6]),
            splits: fit.splits,
            camera_forward: view
                .camera
                .map(|c| c.forward().extend(0.0).to_array())
                .unwrap_or([0.0; 4]),
            camera_pos: view.camera_pos4(),
            map_size: SHADOW_MAP_SIZE as f32,
            enabled: f32::from(self.settings.enabled),
            texel_world: fit.texel_world,
            inv_span: fit.inv_span,
        };
        self.targets
            .ubo(slot)
            .write(0, &pack_shadow_uniforms(&uniform))
            .or_fatal("shadow UBO 写失败，渲染链无法继续")
    }
}

impl PostInput<'_> {
    /// 后处理帧槽 UBO：Bloom 五标量 + SmaaInfo（rt_metrics + 开关）。
    fn write_slot_ubos(&mut self, slot: usize) -> Result<(), BevyError> {
        let bloom = &self.bloom_settings;
        self.bloom
            .ubo(slot)
            .write(
                0,
                &pack_bloom_params(&BloomParamsData {
                    strength: bloom.strength,
                    enabled: f32::from(bloom.enabled),
                    tonemap: f32::from(bloom.tonemap),
                    curve: bloom.curve.as_f32(),
                    gain: bloom.gain,
                }),
            )
            .or_fatal("Bloom 参数 UBO 写失败，渲染链无法继续")?;
        let ldr = self.smaa.ldr_extent();
        let (sw, sh) = (ldr.width.max(1), ldr.height.max(1));
        let mut smaa_info = [0u8; 32];
        put_f32s(
            &mut smaa_info,
            0,
            &[
                1.0 / sw as f32,
                1.0 / sh as f32,
                sw as f32,
                sh as f32,
                f32::from(self.smaa_settings.enabled),
                0.0,
                0.0,
                0.0,
            ],
        );
        self.smaa
            .info_mut(slot)
            .write(0, &smaa_info)
            .or_fatal("SmaaInfo UBO 写失败，渲染链无法继续")
    }
}

impl ClusterInput<'_> {
    /// 写本帧槽的 cluster 均匀数据和光源常驻表脏行。调用时该槽 fence 已等过。
    fn write_slot(
        &mut self,
        slot: usize,
        view: &FrameView,
        extent: vk::Extent2D,
    ) -> Result<(), BevyError> {
        let cluster_uniform = pack_cluster_uniforms(
            &view.view_proj,
            &view.inv_view_proj,
            extent,
            self.clusters.grid(),
            self.lights.light_count(),
        );
        self.clusters
            .uniform(slot)
            .write(0, &cluster_uniform)
            .or_fatal("cluster 均匀数据写失败，渲染链无法继续")?;
        // 只写脏行；一行要在两个帧槽都写过才算清
        self.lights
            .write_dirty(slot)
            .or_fatal("光源常驻表脏行写失败，渲染链无法继续")
    }
}

impl TransparentInput<'_> {
    /// 本帧槽透明行数清零，collect 在 GPU 上从 0 原子累加。
    fn clear_slot_count(&mut self, slot: usize) -> Result<(), BevyError> {
        self.targets
            .count(slot)
            .write(0, &0u32.to_le_bytes())
            .or_fatal("透明行计数清零失败，渲染链无法继续")
    }
}

/// 本帧 compute 段的共享输入。
struct PassFrame<'a> {
    slot: usize,
    /// 对象表已用槽上界（dispatch 线程数与标记环读写长度）。
    bound: u32,
    view_proj: Mat4,
    screen: [f32; 2],
    /// 在场行实例槽号（诊断读数逐行计数的范围）。
    live: &'a [u32],
}

/// 视锥剔除半边：开关决定 compute dispatch 或 host 全可见填充。诊断读数：开 = 本帧槽
/// 两帧前 compute 的输出；关 = host 填充后直读（未 dispatch 时 build_indirect 读的就是
/// 这份填充，dispatch 时 shader 同写 1）。返回 (dispatch, 在场行可见数)。
fn cull_half(
    pf: &PassFrame,
    cull: &mut CullInput,
    shadow_enabled: bool,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<(Option<CullDraw>, u32), BevyError> {
    let bound = pf.bound;
    if bound == 0 {
        return Ok((None, 0));
    }
    // shadow 开启时剔除 dispatch 也要产出 shadow 标记，两侧全关才可跳过。主视图开关随
    // push 进 shader：关 = marks 恒写 1，不覆盖下方 host 全可见填充的语义
    let dispatch = (cull.settings.enabled || shadow_enabled).then(|| {
        first.log(|| info!(
            "视锥剔除 dispatch 首跑：bound {bound}，组 {}，平面随 112B push 进 compute",
            bound.div_ceil(64),
        ));
        cull.pass.draw(&frustum_planes(&pf.view_proj), bound, cull.settings.enabled)
    });
    if !cull.settings.enabled {
        fill_u32(instances.mark_ring(pf.slot), bound as usize, 1)
            .or_fatal("剔除标记环全可见填充失败，渲染链无法继续")?;
    }
    let marks = read_ring(instances.mark_ring(pf.slot), bound as usize * MARK_SIZE as usize)
        .or_fatal("剔除标记环读数失败，渲染链无法继续")?;
    let visible = count_marked(&marks, pf.live);
    cull.diag.enabled = cull.settings.enabled;
    cull.diag.bound = bound;
    cull.diag.marks_frame = pf.slot;
    cull.diag.rows_compared = pf.live.len();
    cull.diag.visible_count = visible;
    Ok((dispatch, visible))
}

/// shadow 标记半边：剔除 dispatch 跑了 → 标记 = compute 输出；shadow 关 → host 全可见
/// 填充（composite 侧 shader 旁路，标记内容无消费）。诊断逐 cascade 计数（陈旧 ≤2 帧，
/// 与主标记相同）。
fn shadow_marks_half(pf: &PassFrame, shadow: &mut ShadowInput) -> Result<(), BevyError> {
    let bound = pf.bound;
    if bound == 0 {
        return Ok(());
    }
    if !shadow.settings.enabled {
        fill_u32(shadow.targets.mark_mut(pf.slot), bound as usize * SHADOW_CASCADES, 1)
            .or_fatal("shadow 标记环全可见填充失败，渲染链无法继续")?;
    }
    let marks = read_ring(
        shadow.targets.mark_mut(pf.slot),
        bound as usize * SHADOW_CASCADES * MARK_SIZE as usize,
    )
    .or_fatal("shadow 标记环读数失败，渲染链无法继续")?;
    let mut gpu_vis = [0u32; SHADOW_CASCADES];
    for &slot in pf.live {
        for (c, vis) in gpu_vis.iter_mut().enumerate() {
            *vis += u32::from(marks.get(slot as usize * SHADOW_CASCADES + c) == Some(&1));
        }
    }
    shadow.diag.enabled = shadow.settings.enabled;
    shadow.diag.bound = bound;
    shadow.diag.marks_frame = pf.slot;
    shadow.diag.gpu_visible = gpu_vis;
    Ok(())
}

/// 遮挡 dispatch 判据（dispatch 与否和 fallback 原因同出一处）。
enum OcclusionVerdict {
    Dispatch,
    /// 跳过 dispatch；原因取首个命中（空场 = None：无可判行，不算 fallback）。
    Skip(Option<OcclusionFallback>),
}

impl OcclusionVerdict {
    fn judge(
        enabled: bool,
        cull_enabled: bool,
        bound: u32,
        pyramid_ready: bool,
        camera_jumped: bool,
    ) -> Self {
        if !enabled {
            Self::Skip(Some(OcclusionFallback::Disabled))
        } else if !cull_enabled {
            Self::Skip(Some(OcclusionFallback::CullOff))
        } else if bound == 0 {
            Self::Skip(None)
        } else if !pyramid_ready {
            Self::Skip(Some(OcclusionFallback::PyramidWarmup))
        } else if camera_jumped {
            Self::Skip(Some(OcclusionFallback::CameraJump))
        } else {
            Self::Dispatch
        }
    }
}

/// Hi-Z 遮挡半边：判据任一命中 → 本帧跳过 dispatch——b3 保持视锥结果、b6 host 填
/// 全可见（读数 = 遮挡剔除量 0）。诊断读数 b6 = 本帧槽两帧前 compute 输出或 host 填充。
fn occlusion_half(
    pf: &PassFrame,
    occlusion: &mut OcclusionInput,
    cull_enabled: bool,
    camera_jumped: bool,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<Option<OcclusionDraw>, BevyError> {
    let bound = pf.bound;
    let verdict = OcclusionVerdict::judge(
        occlusion.settings.enabled,
        cull_enabled,
        bound,
        occlusion.hiz.ready(),
        camera_jumped,
    );
    let (dispatch, fallback) = match verdict {
        OcclusionVerdict::Dispatch => {
            first.log(|| info!(
                "Hi-Z 遮挡测试 dispatch 首跑（复用视锥 compute 链）：bound {bound}，\
                 组 {}，金字塔 {} 级（80B push：view_proj+screen+bound；b3=视锥∧遮挡，b6 遮挡独立标记）",
                bound.div_ceil(64),
                occlusion.hiz.levels(),
            ));
            let draw = occlusion.pass.draw(&pf.view_proj, pf.screen, bound, occlusion.hiz.occlusion_set(pf.slot));
            (Some(draw), None)
        }
        OcclusionVerdict::Skip(reason) => {
            if bound > 0 {
                fill_u32(instances.occ_mark_ring(pf.slot), bound as usize, 1)
                    .or_fatal("遮挡标记环全可见填充失败，渲染链无法继续")?;
            }
            (None, reason)
        }
    };
    let mut visible = 0u32;
    if bound > 0 {
        let marks = read_ring(instances.occ_mark_ring(pf.slot), bound as usize * MARK_SIZE as usize)
            .or_fatal("遮挡标记环读数失败，渲染链无法继续")?;
        visible = count_marked(&marks, pf.live);
    }
    let diag = &mut occlusion.diag;
    diag.enabled = occlusion.settings.enabled;
    diag.dispatched = dispatch.is_some();
    diag.fallback = fallback;
    diag.bound = bound;
    diag.marks_frame = pf.slot;
    diag.rows_compared = pf.live.len();
    diag.occlusion_visible_count = visible;
    Ok(dispatch)
}

/// GPU LOD 选档半边：开关决定 compute dispatch 或 host 档位环填 0（最细档）。选档每帧
/// 按当前相机现算，档位环只是迟滞状态，无需 fallback。诊断：在场行的档位分布（b8 =
/// 本帧槽两帧前 compute 输出或 host 填 0）。
fn lod_half(
    pf: &PassFrame,
    lod: &mut LodInput,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<Option<LodDraw>, BevyError> {
    let bound = pf.bound;
    let dispatch = if lod.settings.enabled && bound > 0 {
        first.log(|| info!(
            "GPU LOD 选档 dispatch 首跑（复用视锥 compute 链）：bound {bound}，\
             组 {}，{MAX_LOD_LEVELS} 档（96B push：view_proj+screen+bound+threshold+hysteresis；\
             b7 变体表按档直址，b8 档位环 = 迟滞状态）",
            bound.div_ceil(64),
        ));
        Some(lod.pass.draw(
            &pf.view_proj,
            pf.screen,
            bound,
            lod.settings.threshold_px,
            lod.settings.hysteresis,
        ))
    } else {
        if bound > 0 {
            fill_u32(instances.lod_level_ring(pf.slot), bound as usize, 0)
                .or_fatal("LOD 档位环全最细档填充失败，渲染链无法继续")?;
        }
        None
    };
    if bound > 0 {
        let levels = read_ring(instances.lod_level_ring(pf.slot), bound as usize * LOD_LEVEL_SIZE as usize)
            .or_fatal("LOD 档位环读数失败，渲染链无法继续")?;
        let mut counts = [0u32; MAX_LOD_LEVELS as usize];
        for slot in pf.live {
            let Some(&level) = levels.get(*slot as usize) else { continue };
            let index = (level as usize).min(counts.len() - 1);
            counts[index] += 1;
        }
        lod.diag.enabled = lod.settings.enabled;
        lod.diag.dispatched = dispatch.is_some();
        lod.diag.bound = bound;
        lod.diag.level_counts = counts;
        lod.diag.rows_compared = pf.live.len();
        lod.diag.marks_frame = pf.slot;
    }
    Ok(dispatch)
}

/// 缺口快照采样：帧起读缺口标记环（本帧槽 = 两帧前 compute 输出）。坑：驻留管理器
/// tick 在帧提交之后跑，直读环会与在飞 compute 写竞争，所以 tick 与 BRP census 都读
/// 本快照。
fn sample_gaps(
    pf: &PassFrame,
    gap: &mut GapSnapshot,
    instances: &mut InstanceTables,
    cull_skipped: bool,
) -> Result<(), BevyError> {
    if pf.bound > 0 {
        let len = pf.bound as usize * GAP_MARK_SIZE as usize;
        gap.bits = read_ring(instances.gap_mark_ring(pf.slot), len)
            .or_fatal("缺口标记环读数失败，渲染链无法继续")?;
        // 缺席槽清零由 cull dispatch 负责（覆盖全部 bound）；本帧未 dispatch（剔除与
        // 阴影都关）时缺席槽会残留陈旧位图，由 host 补零（在场槽由本帧 build_indirect
        // 照常写新值）。坑：补零必须在快照读数之后，先清会抹掉两帧前 build_indirect
        // 写下的缺口，驻留管理器永远看不到
        if cull_skipped {
            instances
                .gap_mark_ring(pf.slot)
                .write(0, &vec![0u8; len])
                .or_fatal("缺口标记环零填充失败，渲染链无法继续")?;
        }
    } else {
        gap.bits.clear();
    }
    gap.bound = pf.bound;
    Ok(())
}

/// indirect 命令组装半边：在场行数 = GPU 命令条数（来自账本，非可见集回读）；空场 =
/// None，场景绘制整段跳过。诊断读数只进计量。
fn indirect_half(
    indirect: &mut IndirectInput,
    count: u32,
    visible: u32,
    walk_rows: usize,
    pending_rows: usize,
    first: &mut OnceLog,
) -> Result<Option<IndirectDraw>, BevyError> {
    let dispatch = if count > 0 {
        first.log(|| info!(
            "indirect 提交首跑：在场行 {count}，命令环 {}B/条，\
             图形侧一次 cmd_draw_indexed_indirect（图形 push range 归零，firstInstance 携带槽号）",
            INDIRECT_COMMAND_SIZE,
        ));
        Some(
            indirect
                .pass
                .draw(count)
                .or_fatal("indirect dispatch 输入组装失败，渲染链无法继续")?,
        )
    } else {
        None
    };
    let stats = &mut indirect.stats;
    stats.draw_calls_recorded = u32::from(count > 0);
    stats.command_count = count;
    stats.zero_instance_count = count - visible;
    stats.walk_rows = walk_rows;
    stats.pending_rows = pending_rows;
    Ok(dispatch)
}

/// 在场行 > 0 才录透明层；在场行数超过透明行容量时报错退出。
fn transparent_draw(
    transparent: &TransparentInput,
    instances: &InstanceTables,
    slot: usize,
    count: u32,
) -> Result<Option<TransparentFrame>, BevyError> {
    if count == 0 {
        return Ok(None);
    }
    let (targets, passes) = (&transparent.targets, &transparent.passes);
    Ok(Some(TransparentFrame {
        transparent_set: targets.frame_set(slot),
        collect: Some(
            passes
                .collect(count)
                .or_fatal("透明行收集 dispatch 输入组装失败，渲染链无法继续")?,
        ),
        gather: Some(passes.gather(count)),
        sort: passes.sort_plan(),
        set2: instances.frame_set2(slot),
        pipeline: transparent.pipeline.rec(),
        t_commands: targets.command_buffer(slot),
        t_keys: targets.key_buffer(slot),
        command_count: count,
    }))
}

/// shadow depth pass 输入：在场行 > 0 且开关开才录四级联绘制；关 = composite
/// 侧 shader 旁路 shadow 采样。
fn shadow_write_draw(
    shadow: &ShadowInput,
    instances: &InstanceTables,
    pool: &MeshPool,
    slot: usize,
    count: u32,
) -> Result<Option<ShadowWriteDraw>, BevyError> {
    if !(shadow.settings.enabled && count > 0) {
        return Ok(None);
    }
    let targets = &shadow.targets;
    Ok(Some(ShadowWriteDraw {
        pipeline: shadow.pipeline.rec(),
        set2: instances.frame_set2(slot),
        shadow_set: targets.frame_set(slot),
        views: std::array::from_fn(|c| targets.map_view(c)),
        images: std::array::from_fn(|c| targets.map_image(c)),
        commands: std::array::from_fn(|c| targets.command(slot, c)),
        command_count: count,
        vertex_buffer: pool
            .vertex_buffer()
            .ok_or_fatal("shadow 输入在场行非空但顶点池缺席——状态矛盾")?,
        index_buffer: pool
            .index_buffer()
            .ok_or_fatal("shadow 输入在场行非空但索引池缺席——状态矛盾")?,
    }))
}

/// Bloom 链录制输入：开关开才录九个全屏 pass；关 = None（tonemap 侧 shader
/// 不采样 bloom，up0 陈旧无消费）。down 集从 level 1 起（level 0 由 prefilter 写）。
fn bloom_draw(post: &PostInput, slot: usize) -> Option<BloomFrameDraw> {
    if !post.bloom_settings.enabled {
        return None;
    }
    let (targets, pipelines) = (&post.bloom, &post.bloom_pipelines);
    Some(BloomFrameDraw {
        prefilter: pipelines.prefilter_rec(),
        down: pipelines.down_rec(),
        up: pipelines.up_rec(),
        gbuffer_set: post.gbuffer.sampled_set(slot),
        down_sets: std::array::from_fn(|i| targets.set_down(i + 1, slot)),
        up_sets: std::array::from_fn(|i| targets.set_up(i, slot)),
        images_down: std::array::from_fn(|i| targets.down_image(i)),
        images_up: std::array::from_fn(|i| targets.up_image(i)),
        views_down: std::array::from_fn(|i| targets.down_view(i)),
        views_up: std::array::from_fn(|i| targets.up_view(i)),
        extents: std::array::from_fn(|i| targets.level_extent(i)),
    })
}

/// SMAA 录制输入：edge/blend 开关开 = Some；neighborhood 恒在（LDR → swapchain
/// 的唯一出口）。
fn smaa_draws(post: &PostInput, slot: usize) -> (Option<SmaaEdgeBlendDraw>, SmaaNbDraw) {
    let (targets, pipelines) = (&post.smaa, &post.smaa_pipelines);
    let edge_blend = post.smaa_settings.enabled.then(|| SmaaEdgeBlendDraw {
        edge: pipelines.edge_rec(),
        blend: pipelines.blend_rec(),
        edge_set: targets.set_edge(slot),
        blend_set: targets.set_blend(slot),
        edges_view: targets.edges_view(slot),
        edges_image: targets.edges_image(slot),
        blend_view: targets.blend_view(slot),
        blend_image: targets.blend_image(slot),
        extent: targets.ldr_extent(),
    });
    let nb = SmaaNbDraw {
        nb: pipelines.neighborhood_rec(),
        nb_set: targets.set_nb(slot),
        ldr_view: targets.ldr_view(slot),
        ldr_image: targets.ldr_image(slot),
        ldr_extent: targets.ldr_extent(),
    };
    (edge_blend, nb)
}

/// host 侧整段填充 u32 环：`count` 个小端 `value`（开关关闭时的全可见/全最细档）。
fn fill_u32(ring: &mut GpuBuffer, count: usize, value: u32) -> Result<(), VulkanError> {
    ring.write(0, &value.to_le_bytes().repeat(count))
}

/// 读 u32 环前 `len` 字节并按小端解包（标记/档位/缺口环同款）：尾部不足 4B 的
/// 残字节丢弃，调用方按槽 `.get()` 取值，越界视同缺席。
fn read_ring(ring: &mut GpuBuffer, len: usize) -> Result<Vec<u32>, VulkanError> {
    let mut bytes = vec![0u8; len];
    ring.read(0, &mut bytes)?;
    Ok(bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect())
}

/// 在场行中标记 = 1 的行数（剔除/遮挡诊断读数）。
fn count_marked(marks: &[u32], live: &[u32]) -> u32 {
    live.iter().filter(|&&slot| marks.get(slot as usize) == Some(&1)).count() as u32
}

/// 清屏色：固定深蓝纯色。线性域值——SRGB view 在 ROP 写出时才做编码
///（render/frame/frame_draw.rs），调色时按线性直觉给值，不要拿 sRGB 十六进制直接换算。
const CLEAR_COLOR: [f32; 4] = [0.02, 0.06, 0.14, 1.0];

/// 灯光日志用的 vec4 格式。
fn fmt_vec4(v: [f32; 4]) -> String {
    format!("({:.1}, {:.1}, {:.1}, {:.1})", v[0], v[1], v[2], v[3])
}

#[cfg(test)]
mod tests {
    use super::OcclusionGate;
    use bevy::math::{Quat, Vec3};

    /// 比较"本帧 vs 两帧前"，阈值作用于两帧累计量：历史不足、累计微动超阈、单帧大跳
    /// 都触发，静止不触发。
    #[test]
    fn occlusion_gate_jump_thresholds() {
        let mut gate = OcclusionGate::default();
        let p0 = Vec3::new(4.0, 2.0, 6.0);
        let r0 = Quat::IDENTITY;
        // 机位历史不足（前两帧）= 未就绪 = 视同跳变（遮挡测试让路）
        assert!(gate.push(Some((p0, r0))), "无历史");
        assert!(gate.push(Some((p0, r0))), "只有一帧历史");
        // 静止相机：不触发
        assert!(!gate.push(Some((p0, r0))), "静止两帧 = 稳定");
        // 单帧小位移（0.3 < 0.5，窗口累计 0.3）：不触发
        assert!(!gate.push(Some((p0 + Vec3::new(0.3, 0.0, 0.0), r0))));
        // 连续微动两帧窗口累计 0.6 > 0.5：触发（金字塔内容已落后累计量）
        assert!(gate.push(Some((p0 + Vec3::new(0.6, 0.0, 0.0), r0))));
        // 大位移（单帧 2.0 > 0.5）触发
        assert!(gate.push(Some((p0 + Vec3::new(2.0, 0.0, 0.0), r0))));
        // 大旋转（16° > 15°）触发
        assert!(gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        // 历史对齐（同位姿再推两帧，清掉大位移残留，使比较基准 = 16° 姿态）
        assert!(gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        assert!(!gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        // 小旋转（两帧窗累计 2° ≤ 15°）不触发
        assert!(!gate.push(Some((p0, Quat::from_rotation_y(14f32.to_radians())))));
        // 相机缺席帧（None = 场景装配问题）视同未就绪
        assert!(gate.push(None));
    }
}
