//! ash 帧循环：一次 update = 一帧——resize 闸门 → 等帧槽位空出 → acquire →
//! 组装 DrawList → 组装帧 UBO（相机/灯光）→ 录制（清屏+绘制）并提交 → present。
//!
//! 资源内聚在各模块：本模块只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
//! 系统注册与禁渲染补位在 [`super::host`]（AshHostPlugin），初始化与拆除链在
//! [`super::init`]。
//!
//! 住址：`Last`，与采集同帧闭环——Update 变更 → PostUpdate 传播+采集
//!（`scene::ledger` 账本对账）→ Last 消费账本：DrawList 槽位枚举与双环写入
//!（[`super::drawlist`]，逐对象参数在 GPU 常驻表——5.1 起）→ 录制提交。
//! GPU 执行异步——上传票据在 GPU 侧等待，CPU 不阻塞；材质按不透明调试
//! 策略覆盖（详见 draw_frame 收账日志）。
//!
//! 帧内错误走统一错误通道（[`crate::common::error::tier_handler`]）：错误点一行
//! `or_warn/or_fatal`（挂 context 负载 + 标 Severity）后 `?` 直冒泡，日志在 handler
//! 统一打、退出裁决在看门 system——Tier① Warning 本帧让路（下帧自愈）；Tier②
//! 优雅退出。两处保持显式分支、不走 `?`：OUT_OF_DATE 是"重试"不是失败（控制流）；
//! present 特例不提前 return（advance 必须照常跑保持帧槽账本一致，就地裁决）。
//! Tier② 的红线：acquire 成功之后的 Vulkan 真失败必须优雅退出——那时同步状态
//! 已无法恢复，warn 后继续会等一个永远无人 signal 的 fence，直接死锁。

use bevy::{
    camera::{Camera, Projection},
    ecs::system::{NonSendMarker, SystemParam},
    light::{AmbientLight, DirectionalLight, GlobalAmbientLight},
    math::Mat4,
    prelude::*,
    window::WindowResized,
};

use ash::vk;

use ash_macros::system;

use crate::{
    common::error::{request_graceful_exit, VulkanError},
    common::syntax::{fatal, OnceLog, ResultTierExt},
    overlay::{paint_overlay, OverlayLogState, RenderMode, UiDrawData},
    scene::{
        GapSnapshot, InstanceLedger, LodGroups, ResidencySettings,
    },
    render::{
        frustum_planes, pack_frame_uniforms, AcquireOutcome, BindlessTables, Context,
        CullDiagnostics, CullPass, CullSettings, FrameDraw, FramePool, FrameSample,
        FrameUniformsData, GraphicsPipeline, GpuTimer, HizPyramid, ImageCache, IndirectPass,
        IndirectStats, InstanceTables, LOD_LEVEL_SIZE, LodDiagnostics, LodPass,
        LodSettings, MAX_LOD_LEVELS, MeshPool, OcclusionDiagnostics, OcclusionPass,
        OcclusionSettings, OverlayPipeline, PerfLedger, Swapchain, Uploader,
        GAP_MARK_SIZE, MAX_FRAMES_IN_FLIGHT, MARK_SIZE,
    },
};

use super::drawlist;
use crate::render::{
    cascade_matrices, pack_cluster_uniforms, pack_shadow_uniforms, ClusterTargets,
    BloomSettings, BloomTargets, CompositePipeline, GBufferTargets, LightTable,
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
    PresentTonemapPipeline, ShadowBuildPass,
    ShadowCascadeInput, ShadowDiagnostics, ShadowPipeline, ShadowSettings, ShadowTargets,
    ShadowUniformsData, ShadowWriteDraw, TransparentFrame, TransparentPasses,
    TransparentPipeline,
    SHADOW_CASCADES, SHADOW_MAP_SIZE,
};

/// resize 闸门状态（`draw_frame` 的 `Local`，跨帧保持）。
///
/// Windows 宿主的两个坑：
/// ① 最小化即发 `Resized(0,0)`，此后对 stale swapchain 的 `acquire` 无限阻塞——
///    阻塞点在 runner 的 `app.update()` 里，消息泵冻死，任务栏"还原"永远点不到。
///    对策：最小化整帧让路（闸门②），不碰 Vulkan，让消息泵活着。
/// ② FIFO present mode 下 acquire 会在显示队列满/表面失配时阻塞，拖拽中尤甚；
///    MAILBOX 让 present 直接替换未上屏帧、acquire 永不排队。
///
/// 尺寸变化只标 pending、帧首按需重建（rebuild 幂等）——acquire 永远落在新
/// swapchain 上，拖拽中帧循环持续流动（宁可渲染卡顿，不要冻结）。
#[derive(Default)]
pub(crate) struct ResizeGate {
    /// 最新一条 resize 消息是 (0,0) = 窗口最小化中；恢复尺寸的非零消息重开闸门
    minimized: bool,
    /// 尺寸变了 / 驱动报次优，下个帧首按需重建（多设无害，rebuild 幂等）
    pending: bool,
    /// 状态转移日志去重：minimized 闸门的开关各报一次，不逐帧刷屏
    logged_minimized: bool,
}

/// 遮挡 fallback 的相机历史（`draw_frame` 的 `Local`，跨帧保持）：跳变判定
/// 用"当前 vs 两帧前"——遮挡读的金字塔正是两帧前的深度（同帧槽 fence 纪律），
/// 对两帧前的机位判稳语义才对齐（阈值钉明见 occlusion 模块注释：位移 0.5 /
/// 旋转 15°，连续缓慢环绕不触发）。
#[derive(Default)]
pub(crate) struct OcclusionGate {
    prev: Option<(Vec3, bevy::math::Quat)>,
    prev_prev: Option<(Vec3, bevy::math::Quat)>,
}

impl OcclusionGate {
    /// 推入本帧机位并返回"相对两帧前是否跳变"（历史不足 = 未就绪 = 视同跳变）。
    fn push(&mut self, now: Option<(Vec3, bevy::math::Quat)>) -> bool {
        let jump = match (self.prev_prev, now) {
            (Some((p0, r0)), Some((p1, r1))) => {
                (p1 - p0).length() > 0.5
                    || bevy::math::Quat::angle_between(r0, r1) > 15f32.to_radians()
            }
            _ => true, // 机位历史不足（启动/相机缺席帧）= 未就绪
        };
        self.prev_prev = self.prev.take();
        self.prev = now;
        jump
    }
}

/// draw_frame 的参数束（SystemParam）：参数束装下成排依赖，系统签名保持精简。
#[derive(SystemParam)]
pub(crate) struct FrameInput<'w, 's> {
    ctx: Res<'w, Context>,
    /// 稳定槽账本（跨帧增量维护；模型环待写集的独占消费点在本系统）。
    ledger: ResMut<'w, InstanceLedger>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    pool: Res<'w, MeshPool>,
    image_cache: Res<'w, ImageCache>,
    /// GPU 常驻绘制数据（对象表/模型环/材质环）：本帧双环写入的写点。
    instance_tables: ResMut<'w, InstanceTables>,
    pipeline: Res<'w, GraphicsPipeline>,
    /// G-buffer 附件 + HDR 中间目标（8.1；闸门③随 swapchain 重建）。
    gbuffer: ResMut<'w, GBufferTargets>,
    /// 合成管线（G-buffer 光照 → HDR 目标，8.1）。
    composite_pipeline: Res<'w, CompositePipeline>,
    /// present tonemap 管线（8.4：HDR + Bloom → ACES → swapchain）。
    tonemap_pipeline: Res<'w, PresentTonemapPipeline>,
    /// Bloom 链资源与三全屏管线（8.4）。
    bloom_targets: ResMut<'w, BloomTargets>,
    bloom_pipelines: Res<'w, crate::render::BloomPipelines>,
    /// SMAA 资源与三全屏管线（8.5）。
    smaa_targets: ResMut<'w, SmaaTargets>,
    smaa_pipelines: Res<'w, SmaaPipelines>,
    smaa_settings: Res<'w, SmaaSettings>,
    bloom_settings: Res<'w, BloomSettings>,
    /// 透明层资源（8.1.3：光源环/uniform/计数的宿主写点 + 集句柄）。
    cluster_targets: ResMut<'w, ClusterTargets>,
    /// 透明层 compute 三件（8.1.3：build/collect/gather dispatch 输入源）。
    transparent_passes: Res<'w, TransparentPasses>,
    /// 透明前向管线（8.1.3）。
    transparent_pipeline: Res<'w, TransparentPipeline>,
    /// 光源常驻表（8.2.2：BRP light_* 方法族的落账面，脏行增量重写）。
    light_table: ResMut<'w, LightTable>,
    /// overlay 管线：UI 半边绑定用（init 链与场景管线同批创建）。
    overlay_pipeline: Res<'w, OverlayPipeline>,
    /// 上传器：普通帧只读票号，overlay 在场时图集整传要走它提交批次（独占写点）。
    uploader: ResMut<'w, Uploader>,
    /// 相机 + 可选的每相机 `AmbientLight` 覆盖（挂了则压过全局资源）。
    cameras: Query<
        'w,
        's,
        (
            &'static Projection,
            &'static GlobalTransform,
            Option<&'static AmbientLight>,
        ),
        With<Camera>,
    >,
    /// 方向光（取第一盏，方向 = 实体 `back()`）。
    lights: Query<'w, 's, (&'static GlobalTransform, &'static DirectionalLight)>,
    ambient: Option<Res<'w, GlobalAmbientLight>>,
    /// 着色模式（overlay 单选钮写；None = overlay 未装，按 lambert 兜底并告警一次）。
    render_mode: Option<Res<'w, RenderMode>>,
    /// overlay 绘制半边资源束（None = overlay 未装，UI 段整段跳过）。
    overlay: Option<UiDrawData<'w>>,
    /// 视锥剔除 compute 管线（5.2；init 链与场景管线同批创建）。
    cull_pass: Res<'w, CullPass>,
    /// 剔除开关（egui checkbox 与 BRP 写；AshHostPlugin 恒插入）。
    cull_settings: Res<'w, CullSettings>,
    /// 剔除诊断账面（每帧刷新，egui/BRP 读数口）。
    cull_diag: ResMut<'w, CullDiagnostics>,
    /// indirect 命令组装 compute 管线（5.3；init 链与场景管线同批创建）。
    indirect_pass: Res<'w, IndirectPass>,
    /// indirect 诊断账面（每帧刷新，egui/BRP 读数口；只进计量）。
    indirect_stats: ResMut<'w, IndirectStats>,
    /// Hi-Z 遮挡测试 compute 管线（6.1；init 链与场景管线同批创建）。
    occlusion_pass: Res<'w, OcclusionPass>,
    /// Hi-Z 金字塔（6.1.2）：归约链录制句柄 + warmup 走账（本帧 note_reduced）。
    hiz: ResMut<'w, HizPyramid>,
    /// 遮挡开关（egui checkbox 与 BRP 写；AshHostPlugin 恒插入）。
    occlusion_settings: Res<'w, OcclusionSettings>,
    /// 遮挡诊断账面（每帧刷新，egui/BRP 读数口；只进计量）。
    occlusion_diag: ResMut<'w, OcclusionDiagnostics>,
    /// GPU LOD 选档 compute 管线（6.2；init 链与场景管线同批创建）。
    lod_pass: Res<'w, LodPass>,
    /// LOD 开关与判据参数（egui checkbox 与 BRP 写；AshHostPlugin 恒插入）。
    lod_settings: Res<'w, LodSettings>,
    /// LOD 诊断账面（每帧刷新，egui/BRP 读数口；只进计量）。
    lod_diag: ResMut<'w, LodDiagnostics>,
    /// shadow 资源族（8.3）：级联 UBO/标记/命令环 + 深度图 + 描述符。
    shadow_targets: ResMut<'w, ShadowTargets>,
    /// shadow 绘制管线（depth-only，8.3）。
    shadow_pipeline: Res<'w, ShadowPipeline>,
    /// shadow 命令组装 compute 管线（8.3）。
    shadow_build_pass: Res<'w, ShadowBuildPass>,
    /// 阴影开关（BRP/egui 写，draw_frame 读）。
    shadow_settings: Res<'w, ShadowSettings>,
    /// 阴影诊断账面（8.3，BRP shadow_stats 读数口）。
    shadow_diag: ResMut<'w, ShadowDiagnostics>,
    /// LOD 组登记处（6.2；只读——LOD 表环写入的变体柄来源）。
    lod_groups: Res<'w, LodGroups>,
    /// 驻留管理器设置（6.3/6.5；DrawList 可绘锚解析读 floor_anchor 开关）。
    residency_settings: Res<'w, ResidencySettings>,
    /// GPU 缺口快照（6.6）：帧起在 fence 纪律内读缺口标记环的采样落点——
    /// 驻留管理器 tick（Last，帧提交后）与 BRP census 经此消费，不直读环。
    gap_snapshot: ResMut<'w, GapSnapshot>,
    /// GPU 帧计时器（5.5；停用时 enabled=false，GPU 区间读数缺席）。
    timer: ResMut<'w, GpuTimer>,
    /// 帧样本账（每帧 push 一份 CPU 分段 + GPU 区间 + 规模；只进计量）。
    perf: ResMut<'w, PerfLedger>,
    /// 组装对照账面（7.9 判定线 2 的 BRP 开关；关闭时零成本）。
    verify: ResMut<'w, super::drawlist::DrawListVerify>,
}

/// DrawList 消费状态（跨帧 `Local`）：缺资源/相机告警去重 + 材质覆盖一次性收账。
#[derive(Default)]
pub(crate) struct DrawListState {
    /// "快照未全部可画（缺驻留/缺材质）"已 warn 过（下帧自愈，不逐帧刷屏）。
    /// 可逆旗标：pending 清零时复位重开（区别于一次性收账的 [`OnceLog`]）。
    warned_pending: bool,
    /// 相机缺席告警只报一次。
    warned_no_camera: OnceLog,
    /// 环境光缺席告警只报一次。
    warned_no_ambient: OnceLog,
    /// 不透明调试覆盖策略报一次（首个完整 DrawList 时）。
    override_logged: OnceLog,
    /// 灯光数据收账报一次（首个取到灯光的帧；值随帧可变，只报"链路已通"）。
    lights_logged: OnceLog,
    /// 多方向光/无方向光的状态告警去重。
    warned_light_count: OnceLog,
    /// RenderMode 资源缺席告警只报一次。
    warned_no_mode: OnceLog,
}

/// ash 帧循环系统（注册与排序在 [`super::host`]）：resize 闸门 → 等帧槽位 →
/// acquire → 组装 DrawList → 帧光照 UBO → overlay 绘制半边 → 录制提交 → present。
///
/// 失败不设壳：错误点 `or_warn/or_fatal` 挂负载标级后 `?` 直冒泡——Err 上交统一
/// 错误通道（[`crate::common::error::tier_handler`] 分流：Warning 本帧让路，其余
/// 置位看门优雅退出），日志在 handler 统一打，本系统只管控制流。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单，逐项声明是框架惯例，非函数签名设计味道"
)]
#[system]
pub(crate) fn draw_frame(
    mut input: FrameInput,
    mut swapchain: ResMut<Swapchain>,
    mut frames: ResMut<FramePool>,
    mut tables: ResMut<BindlessTables>,
    mut resized: MessageReader<WindowResized>,
    mut gate: Local<ResizeGate>,
    mut draw_state: Local<DrawListState>,
    mut occlusion_gate: Local<OcclusionGate>,
    mut paint_log: Local<OverlayLogState>,
    _main_thread: NonSendMarker,
) -> Result<(), BevyError> {
    let overlay = input.overlay.take();
    let FrameInput {
        ref ctx,
        ref mut ledger,
        ref std_materials,
        ref pool,
        ref image_cache,
        ref mut instance_tables,
        ref pipeline,
        ref mut gbuffer,
        ref composite_pipeline,
        ref tonemap_pipeline,
        ref mut bloom_targets,
        ref bloom_pipelines,
        ref mut smaa_targets,
        ref smaa_pipelines,
        ref smaa_settings,
        ref bloom_settings,
        ref mut cluster_targets,
        ref transparent_passes,
        ref transparent_pipeline,
        ref mut light_table,
        ref overlay_pipeline,
        ref cameras,
        ref lights,
        ref ambient,
        ref render_mode,
        ref mut uploader,
        ref cull_pass,
        ref cull_settings,
        ref mut cull_diag,
        ref indirect_pass,
        ref mut indirect_stats,
        ref occlusion_pass,
        ref mut hiz,
        ref occlusion_settings,
        ref mut occlusion_diag,
        ref lod_pass,
        ref lod_settings,
        ref mut lod_diag,
        ref mut shadow_targets,
        ref shadow_pipeline,
        ref shadow_build_pass,
        ref shadow_settings,
        ref mut shadow_diag,
        ref lod_groups,
        ref residency_settings,
        ref mut gap_snapshot,
        ref mut timer,
        ref mut perf,
        ref mut verify,
        ..
    } = input;
    // —— 闸门①：读 resize 消息。一次 update 可能积压多条（拖拽 125Hz 输入 vs 60fps
    // 帧），逐条刷状态，最新一条定生死：非零尺寸 = 正常/恢复，(0,0) = 最小化。
    for msg in resized.read() {
        if msg.width == 0.0 || msg.height == 0.0 {
            gate.minimized = true;
        } else {
            gate.minimized = false;
            gate.pending = true;
        }
    }
    // —— 闸门②：最小化整帧让路（事实①的死锁）。此刻 surface 没有 presentable
    // 尺寸，任何 acquire/present 都可能无限阻塞；直接返回，不碰 Vulkan，
    // runner 继续泵消息，还原点击才能被处理。
    if gate.minimized {
        if !gate.logged_minimized {
            gate.logged_minimized = true;
            info!("窗口最小化：帧循环整帧让路（不碰 swapchain），消息泵保持存活");
        }
        return Ok(());
    }
    if gate.logged_minimized {
        gate.logged_minimized = false;
        info!("窗口脱离最小化：闸门重开");
    }
    // —— 闸门③：尺寸变化 → 帧首按需重建，时机在 acquire 之前。rebuild 幂等
    //（尺寸没变就空手而归），拖拽中每步一建、帧循环不断流——acquire 永远落在
    // 新 swapchain 上。深度附件随其后按新 extent 重建（同样幂等；重建自带
    // device_wait_idle，在飞旧深度不可能被引用）。pending 保持置位、两重建都
    // 成功才清：重建失败的帧在 or_warn 处让路，下个帧首原样重试。
    if gate.pending {
        swapchain
            .rebuild(ctx)
            .or_warn("resize 后重建失败，下帧重试")?;
        frames
            .rebuild_depth(ctx, swapchain.extent)
            .or_fatal("深度附件重建失败，渲染链无法继续")?;
        // Hi-Z 金字塔随深度附件同批重建（同样幂等；自带 device_wait_idle，
        // 重建后 warmup 重置——遮挡测试 fallback 到金字塔重新就绪）
        let depth_images: Vec<vk::Image> =
            (0..MAX_FRAMES_IN_FLIGHT).map(|s| frames.depth_image(s)).collect();
        hiz.rebuild(ctx, swapchain.extent, &depth_images)
            .or_fatal("Hi-Z 金字塔重建失败，渲染链无法继续")?;
        // Bloom 链随 swapchain 同批重建（同样幂等；自带 device_wait_idle）
        bloom_targets
            .rebuild(ctx, swapchain.extent)
            .or_fatal("Bloom 链重建失败，渲染链无法继续")?;
        // G-buffer 附件随 swapchain 同批重建（幂等；自带 device_wait_idle；
        // 深度视图已按新尺寸重建——采样集 b3 的重建依赖）
        let depth_views: Vec<vk::ImageView> =
            (0..MAX_FRAMES_IN_FLIGHT).map(|s| frames.depth_view(s)).collect();
        gbuffer
            .rebuild(ctx, swapchain.extent, &depth_views)
            .or_fatal("G-buffer 附件重建失败，渲染链无法继续")?;
        // 透明层 cluster 表随 swapchain 同批重建（幂等；自带 device_wait_idle；
        // 光源常驻表环不在重建域——容量与尺寸无关，描述符重绑由 rebuild 接收）
        let light_ring_buffers: Vec<vk::Buffer> = (0..MAX_FRAMES_IN_FLIGHT)
            .map(|s| light_table.ring_buffer(s))
            .collect();
        cluster_targets
            .rebuild(ctx, swapchain.extent, &light_ring_buffers)
            .or_fatal("透明层 cluster 表重建失败，渲染链无法继续")?;
        // SMAA 三目标（LDR/edges/blend）随 swapchain 同批重建（幂等；自带
        // device_wait_idle；描述符集随 build_descriptors 重写）——resize 后
        // tonemap/edge/blend 的 renderArea 与附件尺寸必须同帧对齐
        smaa_targets
            .rebuild(ctx, swapchain.extent)
            .or_fatal("SMAA 三目标重建失败，渲染链无法继续")?;
        gate.pending = false;
    }

    // 1) 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在下方提交前一刻）。
    // 失败 = 等待/重置层面坏掉（设备丢失、内存枯竭），继续帧循环只会撞上未知的
    // fence 状态——Tier②
    let frame_wall_ms = perf.mark_frame_start();
    let frame_clock = std::time::Instant::now();
    frames
        .wait_for_slot()
        .or_fatal("帧槽等待/重置失败，渲染链无法继续")?;
    let wait_slot_ms = frame_clock.elapsed().as_secs_f32() * 1000.0;
    // —— GPU 帧计时取回（5.5）：本槽 fence 已等完 = 上轮提交执行完毕，4 位
    // timestamp 可读。混合口径如实标注：GPU 区间是本槽上轮（两帧前）的提交，
    // 稳态下与当前帧同构；停用时恒 0。
    let gpu_sections = timer
        .fetch(frames.current_index())
        .or_fatal("GPU 帧计时取回失败，渲染链无法继续")?
        .unwrap_or([0.0; 8]);

    // 2) acquire：拿到一张可画的 image；OUT_OF_DATE = 这个 swapchain 已不可用
    //（最小化/恢复/独占模式切换等），必须立即重建——等不得下一帧
    let acquire_clock = std::time::Instant::now();
    let index = match swapchain.acquire(frames.current().image_available) {
        Ok(AcquireOutcome::Ready(index)) => index,
        Ok(AcquireOutcome::Suboptimal(index)) => {
            // 拿得到图但尺寸不理想：照常画完这帧，标 pending——下个帧首重建
            gate.pending = true;
            index
        }
        Err(VulkanError::SwapchainOutOfDate) => {
            // 无论重建成败，本帧作废（acquire 没拿到图）；重建失败 warn 后让路，
            // 下帧 acquire 再过时就地重试重建。重建后必须置 pending——
            // rebuild 对"尺寸没变"的表面失配（显示器模式切换/热插拔类）幂等
            // 早退，不置位则本分支每帧空转重试、渲染不自愈；置位后帧首闸门③
            // 接管（rebuild 幂等，重复调用无成本）
            swapchain.rebuild(ctx).or_warn("acquire 过时后重建失败")?;
            gate.pending = true;
            return Ok(());
        }
        Err(e) => return Err(fatal("acquire 失败，渲染链无法继续", e)),
    };
    let acquire_ms = acquire_clock.elapsed().as_secs_f32() * 1000.0;

    // 3) 组装 DrawList（7.9 增量维护）：账本组装缓存消费重估集（变更点 =
    // 行增删/换绑/置脏/槽位交接/驻留出账的各钩子），行宇宙稳定帧零走访。
    // 可绘判定与 4.x 语义一致：实例槽缺/mesh 未驻留/材质缺席 = 暂缓
    //（Tier① 自愈）。暂缓行数 = 行总数 − 可绘数（缓存维护后恒成立）。
    let assemble_clock = std::time::Instant::now();
    drawlist::assemble_update(ledger, pool, std_materials, lod_groups, residency_settings.floor_anchor);
    let assemble_ms = assemble_clock.elapsed().as_secs_f32() * 1000.0;
    // 对照模式（7.9 判定线 2）：BRP 开关开启时逐帧跑全量参照走访与增量
    // 缓存逐字段比对——只进计量，不驱动绘制（诊断性回读边界照 5.2 拍板）。
    if verify.enabled {
        drawlist::verify_assembly(
            ledger,
            pool,
            std_materials,
            lod_groups,
            residency_settings.floor_anchor,
            ledger.assembly_pending(),
            verify,
        );
    }
    // —— 奇偶集先于视图取走（写侧的账本可变消费点收口），此后 assembly
    // 视图对账本共享借用，环写入与诊断读数全程零可变访问。
    let model_dirty = ledger.take_model_dirty(frames.current_index());
    let lod_dirty = ledger.take_lod_dirty(frames.current_index());
    let assembly = ledger.assembly_view();
    let pending_rows = ledger.len() - assembly.slots.len();
    if pending_rows > 0 {
        if !draw_state.warned_pending {
            draw_state.warned_pending = true;
            info!(
                "DrawList 暂缓 {}/{} 行（实例槽/mesh/材质未就绪，上传链自愈后消失）",
                pending_rows,
                ledger.len(),
            );
        }
    } else {
        draw_state.warned_pending = false;
    }
    if !ledger.is_empty() && pending_rows == 0 {
        draw_state.override_logged.log(|| info!(
            "材质调试覆盖：{} 个 primitive 全部按不透明绘制——blend 关闭（alpha<1 的 {} 行照常画出，glTF BLEND 镜片在此列）；材质参数/贴图槽走材质环（每帧从容器现读）；着色模式按 ASH_RENDER_MODE（三态，见灯光收账日志）",
            ledger.len(),
            assembly.alpha_override,
        ));
    }

    // 4) 相机/灯光 → 本帧槽 UBO。复用安全 = wait_for_slot 的 fence：写
    // 发生在上一轮使用完成之后（帧槽轮转只是定位，安全证据是 fence）。
    // 相机缺席是场景装配问题，报一次后 identity 兜底（画面不可读但不崩帧循环）。
    // 矩阵乘序/列主序与 WGSL 端同一约定。材质模式来自 overlay 的单选钮资源
    //（初值 = env ASH_RENDER_MODE）。
    let render_mode = match render_mode.as_deref() {
        Some(mode) => *mode,
        None => {
            if !ledger.is_empty() {
                draw_state.warned_no_mode.log(|| {
                    warn!("RenderMode 资源缺席（overlay 未装）：材质模式按 lambert 兜底")
                });
            }
            RenderMode::Lambert
        }
    };
    let mut view_proj = Mat4::IDENTITY;
    let mut inv_view_proj = Mat4::IDENTITY;
    let mut per_camera_ambient: Option<&AmbientLight> = None;
    let mut cam_now: Option<(Vec3, bevy::math::Quat)> = None;
    // 8.3：相机投影 near（camera match 内写入；默认值兜底相机缺席帧）
    let mut camera_near: f32 = 0.1;
    match cameras.single() {
        Ok((projection, global, ambient_override)) => {
            view_proj = projection.get_clip_from_view() * global.affine().inverse();
            // 8.1.2 深度重建常量面：clip→world 反投影（与 view_proj 同帧同源；
            // glam 的 Mat4::inverse 通用逆，投影矩阵可逆性由投影构造保证）
            inv_view_proj = view_proj.inverse();
            per_camera_ambient = ambient_override;
            cam_now = Some((global.translation(), global.rotation()));
            // 8.3 级联底座：投影 near（z_ndc↔视深换算）；非透视投影兜底 100
            camera_near = match projection {
                Projection::Perspective(p) => p.near,
                _ => 100.0,
            };
        }
        Err(_) => draw_state
            .warned_no_camera
            .log(|| warn!("相机缺席：view_proj 用 identity 兜底（画面不可读属预期，查相机装配）")),
    }
    // —— 方向光：取第一盏（多灯告警一次，多灯支持未实现）。
    // 数值约定：dir_to_light = 实体 back()（forward 取
    // 负，"N·L 就绪"），light_color = linear × illuminance。
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
        None => (
            crate::render::PLACEHOLDER_DIR_TO_LIGHT,
            [0.0; 4],
        ), // 无灯：直射项归零，方向留合法占位（共享常量见 descriptors.rs）
    };
    if light_count != 1 && !ledger.is_empty() {
        draw_state.warned_light_count.log(|| {
            warn!("方向光 {light_count} 盏（取第一盏/无灯直射项归零），多灯支持未实现")
        });
    }
    // —— 环境光：相机组件 AmbientLight（若挂）压过全局 GlobalAmbientLight 资源；
    // 数值 = linear × brightness。
    let ambient_color = match per_camera_ambient {
        Some(a) => LinearRgba::from(a.color).to_f32_array().map(|c| c * a.brightness),
        None => match ambient.as_deref() {
            Some(global) => {
                LinearRgba::from(global.color).to_f32_array().map(|c| c * global.brightness)
            }
            None => {
                draw_state.warned_no_ambient.log(|| {
                    warn!("环境光缺席：GlobalAmbientLight 资源与相机 AmbientLight 均无，环境项归零")
                });
                [0.0; 4]
            }
        },
    };
    if light_count > 0 {
        draw_state.lights_logged.log(|| info!(
            "灯光 UBO 收账：方向光 {light_count} 盏（dir_to_light = back()，light = {}）；\
             环境 = {}（来源 {}）；材质模式 = {}（ASH_RENDER_MODE）",
            fmt_vec4(light_color),
            fmt_vec4(ambient_color),
            if per_camera_ambient.is_some() { "相机 AmbientLight" } else { "GlobalAmbientLight" },
            match render_mode {
                RenderMode::Lambert => "filament",
                RenderMode::Unlit => "unlit",
                RenderMode::Normal => "normal",
                RenderMode::Position => "position",
                RenderMode::Metallic => "metallic",
                RenderMode::Roughness => "roughness",
                RenderMode::AmbientOcclusion => "ao",
                RenderMode::Reflectance => "reflectance",
                RenderMode::Depth => "depth",
            },
        ));
    }
    tables
        .frame_ubo(frames.current_index())
        .write(
            0,
            &pack_frame_uniforms(&FrameUniformsData {
                view_proj,
                inv_view_proj,
                dir_to_light,
                ambient_color,
                light_color,
                mode: render_mode.as_u32(),
                // 8.2.3：光照累积 V 向量的相机世界位置（相机缺席帧 zeros 兜底）
                camera_pos: cam_now
                    .map(|(t, _)| [t.x, t.y, t.z, 0.0])
                    .unwrap_or([0.0; 4]),
            }),
        )
        .or_fatal("帧 UBO 写失败，渲染链无法继续")?;

    // 4a-0-1) shadow UBO（8.3，帧槽环与主 UBO 同一 fence 证据）：级联矩阵
    //（log 分割拟合）+ 四×六平面（cull 内联标记与 shadow_stats CPU 参照同源）
    //+ 分割带/相机基（composite cascade 选择）。相机缺席帧 = identity 矩阵
    //（画面不可读属预期，与帧 UBO 兜底同口径）。
    let shadow_frame = frames.current_index();
    let (shadow_cascades, shadow_splits) = match cam_now {
        Some((pos, rot)) => {
            let forward = rot * Vec3::NEG_Z;
            let input = ShadowCascadeInput {
                inv_view_proj,
                camera_pos: pos,
                camera_forward: forward,
                near: camera_near,
                dir_to_light: Vec3::from_slice(&dir_to_light[..3]).normalize_or_zero(),
            };
            cascade_matrices(&input)
        }
        None => ([Mat4::IDENTITY; SHADOW_CASCADES], [f32::INFINITY; SHADOW_CASCADES]),
    };
    let shadow_planes: Vec<Vec4> = shadow_cascades
        .iter()
        .flat_map(|m| frustum_planes(m).to_vec())
        .collect();
    let shadow_uniform = ShadowUniformsData {
        cascade_view_proj: shadow_cascades,
        planes: shadow_planes.try_into().expect("4×6 平面"),
        splits: shadow_splits,
        camera_forward: cam_now
            .map(|(_, r)| {
                let f = r * Vec3::NEG_Z;
                [f.x, f.y, f.z, 0.0]
            })
            .unwrap_or([0.0; 4]),
        camera_pos: cam_now.map(|(t, _)| [t.x, t.y, t.z, 0.0]).unwrap_or([0.0; 4]),
        map_size: SHADOW_MAP_SIZE as f32,
        enabled: f32::from(shadow_settings.enabled),
    };
    shadow_targets
        .ubo(shadow_frame)
        .write(0, &pack_shadow_uniforms(&shadow_uniform))
        .or_fatal("shadow UBO 写失败，渲染链无法继续")?;

    // 4a-0-2) Bloom 参数 UBO（8.4，帧槽环与主 UBO 同一 fence 证据）：
    // strength/enabled/tonemap/curve/gain 五标量——BRP bloom_set 直达。
    let bloom_frame = frames.current_index();
    bloom_targets
        .ubo(bloom_frame)
        .write(
            0,
            &crate::render::pack_bloom_params(&crate::render::BloomParamsData {
                strength: bloom_settings.strength,
                enabled: f32::from(bloom_settings.enabled),
                tonemap: f32::from(bloom_settings.tonemap),
                curve: bloom_settings.curve.as_f32(),
                gain: bloom_settings.gain,
            }),
        )
        .or_fatal("Bloom 参数 UBO 写失败，渲染链无法继续")?;

    // 4a-0-3) SmaaInfo UBO（8.5，帧槽环同一 fence 证据）：rt_metrics + 开关。
    let smaa_frame = frames.current_index();
    let (sw, sh) = (smaa_targets.ldr_extent().width.max(1), smaa_targets.ldr_extent().height.max(1));
    let mut smaa_info = [0u8; 32];
    for (k, v) in [
        1.0 / sw as f32,
        1.0 / sh as f32,
        sw as f32,
        sh as f32,
        f32::from(smaa_settings.enabled),
        0.0,
        0.0,
        0.0,
    ]
    .iter()
    .enumerate()
    {
        smaa_info[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    smaa_targets
        .info_mut(smaa_frame)
        .write(0, &smaa_info)
        .or_fatal("SmaaInfo UBO 写失败，渲染链无法继续")?;

    // 4a-0) 透明层帧侧宿主写（8.1.3，wait_for_slot 的 fence 纪律内）：
    // cluster 均匀数据（view_proj/inv_view_proj/网格标量）、光源常驻表脏行
    //（8.2.2）、透明行计数清零（collect 的 atomicAdd 基线）。
    let slot_now = frames.current_index();
    let cluster_uniform = pack_cluster_uniforms(
        &view_proj,
        &inv_view_proj,
        swapchain.extent,
        cluster_targets.grid(),
        light_table.light_count(),
    );
    cluster_targets
        .uniform(slot_now)
        .write(0, &cluster_uniform)
        .or_fatal("cluster 均匀数据写失败，渲染链无法继续")?;
    // 光源常驻表脏行增量重写（8.2.2，wait_for_slot 之后——fence 纪律同模型
    // 环；稳态零写，变更帧只写脏行且覆盖两个帧槽才出账）
    light_table
        .write_dirty(slot_now)
        .or_fatal("光源常驻表脏行写失败，渲染链无法继续")?;
    // prev 透明计数先读后清（本帧槽两帧前输出，fence 纪律）——bitonic 排序
    // 的空排让路判据（prev = 0 的帧数组未排，gather 尾部反向读取仍覆盖全部
    // 真实行，只丢一帧排序；读数不决定画什么，只决定是否跑空排序）
    let mut prev_t_bytes = [0u8; 4];
    cluster_targets
        .t_count(slot_now)
        .read(0, &mut prev_t_bytes)
        .or_fatal("透明行计数读失败，渲染链无法继续")?;
    let prev_transparent_rows = u32::from_le_bytes(prev_t_bytes);
    cluster_targets
        .t_count(slot_now)
        .write(0, &0u32.to_le_bytes())
        .or_fatal("透明行计数清零失败，渲染链无法继续")?;

    // 4a) 双环写入（wait_for_slot 之后，与帧 UBO 同一条 fence 证据）：模型环
    // 增量重写（本帧奇偶集的改动行）+ 材质环全量重写（可绘行的材质从容器
    // 现读）。见 instance_table 模块文档的 hazard 纪律表。奇偶集在组装段
    // 之后取走（借用纪律见步骤 3 注释），assembly 视图 = 账本共享借用。
    let rings_clock = std::time::Instant::now();
    drawlist::write_rings(
        instance_tables,
        ledger,
        std_materials,
        image_cache,
        &model_dirty,
        &lod_dirty,
        &assembly,
        frames.current_index(),
        lod_groups,
        pool,
    )
    .or_fatal("模型/材质/在场/LOD 环写失败，渲染链无法继续")?;
    let rings_ms = rings_clock.elapsed().as_secs_f32() * 1000.0;

    // 4a-2) 视锥剔除半边（5.2）：开关决定 compute dispatch 或 host 全可见填充
    //（数据层 A/B）；诊断读数在写入路径之后取——标记环读写都在 wait_for_slot
    // 的 fence 纪律内（与环同链，cull 模块文档）。诊断只进计量（判定线 2
    // 边界：任何系统不得读它决定画什么）。
    let cull_frame = frames.current_index();
    let cull_bound = instance_tables.used_object_slots();
    // 8.3：shadow 开启时剔除 dispatch 也承担 shadow 标记产出——即使主视锥
    // A/B 关，dispatch 仍要跑（写 shadow 标记）；两侧全关才可跳过
    let cull_dispatch = if (cull_settings.enabled || shadow_settings.enabled) && cull_bound > 0 {
        // 闸门④空转首跑留痕：首个 dispatch 帧收账一次（零 VUID 由验证层执法）
        if !cull_diag.first_dispatch_logged {
            info!(
                "视锥剔除 dispatch 首跑（前置闸门④）：bound {cull_bound}，组 {}，平面随 112B push 进 compute",
                cull_bound.div_ceil(64),
            );
            cull_diag.first_dispatch_logged = true;
        }
        Some(cull_pass.draw(&frustum_planes(&view_proj), cull_bound))
    } else {
        None
    };
    // 诊断读数：开 = 本帧槽两帧前 compute 的输出（最新确定值）；关 = host
    // 全可见填充后直读（A/B 的 B 面基准）。逐在场行计数。
    if !cull_settings.enabled && cull_bound > 0 {
        let ones: Vec<u8> = (0..cull_bound).flat_map(|_| 1u32.to_le_bytes()).collect();
        instance_tables
            .mark_ring(cull_frame)
            .write(0, &ones)
            .or_fatal("剔除标记环全可见填充失败，渲染链无法继续")?;
    }
    let mut visible_among_live = 0u32;
    if cull_bound > 0 {
        let mut bytes = vec![0u8; cull_bound as usize * MARK_SIZE as usize];
        instance_tables
            .mark_ring(cull_frame)
            .read(0, &mut bytes)
            .or_fatal("剔除标记环读数失败，渲染链无法继续")?;
        let mark_at =
            |slot: u32| u32::from_le_bytes(bytes[slot as usize * 4..slot as usize * 4 + 4].try_into().expect("4B"));
        visible_among_live = assembly
            .slots
            .iter()
            .filter(|&&slot| mark_at(slot) == 1)
            .count() as u32;
        cull_diag.enabled = cull_settings.enabled;
        cull_diag.bound = cull_bound;
        cull_diag.marks_frame = cull_frame;
        cull_diag.rows_compared = assembly.slots.len();
        cull_diag.visible_count = visible_among_live;
    }

    // 4a-2-0) shadow 标记半边（8.3）：dispatch 跑了 → 标记 = compute 输出；
    // 没跑（shadow 关或空场）→ host 全可见填充（读数语义 = 剔除量 0，A/B 的
    // B 面同口径；composite 侧 shadow 关 = shader 旁路，标记内容无消费）。
    // 诊断逐 cascade 计数（fence 纪律内直读，陈旧 ≤2 帧与主标记同款）。
    if !(shadow_settings.enabled && cull_bound > 0) {
        let ones: Vec<u8> = (0..cull_bound)
            .flat_map(|_| [1u32, 1u32, 1u32, 1u32].map(u32::to_le_bytes).concat())
            .collect();
        shadow_targets
            .mark_mut(shadow_frame)
            .write(0, &ones)
            .or_fatal("shadow 标记环全可见填充失败，渲染链无法继续")?;
    }
    if cull_bound > 0 {
        let mut sbytes = vec![0u8; cull_bound as usize * 4 * 4];
        shadow_targets
            .mark_mut(shadow_frame)
            .read(0, &mut sbytes)
            .or_fatal("shadow 标记环读数失败，渲染链无法继续")?;
        let smark_at = |slot: u32, c: usize| {
            u32::from_le_bytes(
                sbytes[slot as usize * 16 + c * 4..slot as usize * 16 + c * 4 + 4]
                    .try_into()
                    .expect("4B"),
            )
        };
        let mut gpu_vis = [0u32; SHADOW_CASCADES];
        for &slot in assembly.slots.iter() {
            for (c, vis) in gpu_vis.iter_mut().enumerate() {
                *vis += u32::from(smark_at(slot, c) == 1);
            }
        }
        shadow_diag.enabled = shadow_settings.enabled;
        shadow_diag.bound = cull_bound;
        shadow_diag.marks_frame = shadow_frame;
        shadow_diag.gpu_visible = gpu_vis;
        shadow_diag.cpu_visible = [0; SHADOW_CASCADES];
    }

    // 4a-2-1) Hi-Z 遮挡半边（6.1）：fallback 判据逐项核对（occlusion 模块
    // 注释钉明的四条），任一命中 → 本帧跳过 dispatch——b3 保持视锥纯结果、
    // b6 host 填全可见（读数语义 = 遮挡剔除量 0，A/B 的 B 面同口径）。跳变
    // 判定用"当前 vs 两帧前"机位（OcclusionGate——遮挡读的金字塔正是两帧前
    // 的深度，判定线 2 的显式 fallback 落点）。
    let camera_jumped = occlusion_gate.push(cam_now);
    let occl_dispatch = if occlusion_settings.enabled
        && cull_settings.enabled
        && cull_bound > 0
        && hiz.ready()
        && !camera_jumped
    {
        if !occlusion_diag.first_run_logged {
            info!(
                "Hi-Z 遮挡测试 dispatch 首跑（前置闸门复用 5.2 compute 链）：bound {cull_bound}，\
                 组 {}，金字塔 {} 级（80B push：view_proj+screen+bound；b3=视锥∧遮挡，b6 遮挡独立标记）",
                cull_bound.div_ceil(64),
                hiz.levels(),
            );
            occlusion_diag.first_run_logged = true;
        }
        Some(occlusion_pass.draw(
            &view_proj,
            [
                swapchain.extent.width as f32,
                swapchain.extent.height as f32,
            ],
            cull_bound,
            hiz.occlusion_set(cull_frame),
        ))
    } else {
        if cull_bound > 0 {
            let ones: Vec<u8> = (0..cull_bound).flat_map(|_| 1u32.to_le_bytes()).collect();
            instance_tables
                .occ_mark_ring(cull_frame)
                .write(0, &ones)
                .or_fatal("遮挡标记环全可见填充失败，渲染链无法继续")?;
        }
        None
    };
    // 遮挡诊断读数（b6 = 本帧槽两帧前 compute 输出或 host 全可见填充；fence
    // 纪律与标记环同链）。fallback 原因取首个命中（读数口径，不改控制流）。
    let mut occlusion_visible = 0u32;
    if cull_bound > 0 {
        let mut occ_bytes = vec![0u8; cull_bound as usize * MARK_SIZE as usize];
        instance_tables
            .occ_mark_ring(cull_frame)
            .read(0, &mut occ_bytes)
            .or_fatal("遮挡标记环读数失败，渲染链无法继续")?;
        occlusion_visible = assembly
            .slots
            .iter()
            .filter(|&&slot| {
                u32::from_le_bytes(
                    occ_bytes[slot as usize * 4..slot as usize * 4 + 4]
                        .try_into()
                        .expect("4B"),
                ) == 1
            })
            .count() as u32;
    }
    occlusion_diag.enabled = occlusion_settings.enabled;
    occlusion_diag.dispatched = occl_dispatch.is_some();
    occlusion_diag.fallback = if occl_dispatch.is_some() {
        None
    } else if !occlusion_settings.enabled {
        Some(crate::render::OcclusionFallback::Disabled)
    } else if !cull_settings.enabled {
        Some(crate::render::OcclusionFallback::CullOff)
    } else if cull_bound == 0 {
        None // 空场：无可判行，不算 fallback（画面语义 = 本来就无对象）
    } else if !hiz.ready() {
        Some(crate::render::OcclusionFallback::PyramidWarmup)
    } else if camera_jumped {
        Some(crate::render::OcclusionFallback::CameraJump)
    } else {
        None
    };
    occlusion_diag.bound = cull_bound;
    occlusion_diag.marks_frame = cull_frame;
    occlusion_diag.rows_compared = assembly.slots.len();
    occlusion_diag.occlusion_visible_count = occlusion_visible;

    // 4a-2-2) GPU LOD 选档半边（6.2）：开关决定 compute dispatch 或 host 档位
    // 环填 0（数据层 A/B；选档每帧从当前相机现算，无 fallback 专段——档位
    // 环只是迟滞状态不是真值，lod 模块注释钉明）。诊断读数在写入路径之后
    // 取——档位环读写都在 wait_for_slot 的 fence 纪律内。
    let lod_dispatch = if lod_settings.enabled && cull_bound > 0 {
        if !lod_diag.first_run_logged {
            info!(
                "GPU LOD 选档 dispatch 首跑（前置闸门复用 5.2 compute 链）：bound {cull_bound}，\
                 组 {}，{MAX_LOD_LEVELS} 档（96B push：view_proj+screen+bound+threshold+hysteresis；\
                 b7 变体表按档直址，b8 档位环 = 迟滞状态）",
                cull_bound.div_ceil(64),
            );
            lod_diag.first_run_logged = true;
        }
        Some(lod_pass.draw(
            &view_proj,
            [swapchain.extent.width as f32, swapchain.extent.height as f32],
            cull_bound,
            lod_settings.threshold_px,
            lod_settings.hysteresis,
        ))
    } else {
        if cull_bound > 0 {
            let zeros: Vec<u8> = (0..cull_bound).flat_map(|_| 0u32.to_le_bytes()).collect();
            instance_tables
                .lod_level_ring(cull_frame)
                .write(0, &zeros)
                .or_fatal("LOD 档位环全最细档填充失败，渲染链无法继续")?;
            // cull 关 = 清零段不跑，缺口环缺席槽会残留陈旧位图——host 补零
            //（在场槽由本帧 build_indirect 照常写新鲜值）
            instance_tables
                .gap_mark_ring(cull_frame)
                .write(0, &zeros)
                .or_fatal("缺口标记环零填充失败，渲染链无法继续")?;
        }
        None
    };
    // LOD 诊断读数：档位分布逐在场行统计（b8 = 本帧槽两帧前 compute 输出或
    // host 填 0；fence 纪律与标记环同链）。
    if cull_bound > 0 {
        let mut lod_bytes = vec![0u8; cull_bound as usize * LOD_LEVEL_SIZE as usize];
        instance_tables
            .lod_level_ring(cull_frame)
            .read(0, &mut lod_bytes)
            .or_fatal("LOD 档位环读数失败，渲染链无法继续")?;
        let mut counts = [0u32; MAX_LOD_LEVELS as usize];
        for slot in assembly.slots {
            let level = u32::from_le_bytes(
                lod_bytes[*slot as usize * 4..*slot as usize * 4 + 4].try_into().expect("4B"),
            );
            let index = (level as usize).min(counts.len() - 1);
            counts[index] += 1;
        }
        lod_diag.enabled = lod_settings.enabled;
        lod_diag.dispatched = lod_dispatch.is_some();
        lod_diag.bound = cull_bound;
        lod_diag.level_counts = counts;
        lod_diag.rows_compared = assembly.slots.len();
        lod_diag.marks_frame = cull_frame;
    }

    // 6.6 缺口快照采样：帧起读缺口标记环（本帧槽 = 两帧前 compute 输出，
    // fence 纪律同档位环诊断）——驻留管理器 tick 挂 Last 在帧提交之后，直读
    // 环会与在飞 compute 写竞争，消费口统一走本快照（tick 与 BRP census）。
    if cull_bound > 0 {
        let mut gap_bytes = vec![0u8; cull_bound as usize * GAP_MARK_SIZE as usize];
        instance_tables
            .gap_mark_ring(cull_frame)
            .read(0, &mut gap_bytes)
            .or_fatal("缺口标记环读数失败，渲染链无法继续")?;
        gap_snapshot.bits = gap_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
    } else {
        gap_snapshot.bits.clear();
    }
    gap_snapshot.bound = cull_bound;

    // 4a-3) indirect 命令组装半边（5.3）：在场行数 = GPU 命令条数（CPU 在账
    // 信息，非可见集回读）；count > 0 才有命令可组装（空场 = None，场景绘制
    // 整段跳过）。诊断读数（draw 调用数/命令条数/instanceCount=0 条数）只进
    // 计量，单独标注（诊断性回读边界照 5.2 拍板）。
    let indirect_count = assembly.slots.len() as u32;
    let indirect_dispatch = if indirect_count > 0 {
        if !indirect_stats.first_run_logged {
            info!(
                "indirect 提交首跑（前置闸门②）：在场行 {indirect_count}，命令环 {}B/条，\
                 图形侧一次 cmd_draw_indexed_indirect（push 已退役，firstInstance 携带槽号）",
                crate::render::INDIRECT_COMMAND_SIZE,
            );
            indirect_stats.first_run_logged = true;
        }
        Some(
            indirect_pass
                .draw(indirect_count)
                .or_fatal("indirect dispatch 输入组装失败，渲染链无法继续")?,
        )
    } else {
        None
    };
    indirect_stats.draw_calls_recorded = u32::from(indirect_count > 0);
    indirect_stats.command_count = indirect_count;
    indirect_stats.zero_instance_count = indirect_count - visible_among_live;
    indirect_stats.walk_rows = ledger.len();
    indirect_stats.pending_rows = pending_rows;

    // 4a-3-1) shadow 命令组装半边（8.3）：读 shadow 标记 + LOD 档位环，逐在场
    // 行 × 每 cascade 组装命令（两枚语义钉：无遮挡参与、LOD 再粗一档）。空场
    // /关闭 = None（shadow 命令环内容无消费——shadow pass 整段跳过）。
    let shadow_build_dispatch = if shadow_settings.enabled && indirect_count > 0 {
        Some(shadow_build_pass.draw(indirect_count))
    } else {
        None
    };

    // 4b) overlay 绘制半边：图集整传（可能多出一张 transfer 票据——本帧
    // 图形提交的 wait_ticket 在此后快照，flush_uploads 批与图集批都被等待）→
    // 镶嵌 → 顶点环写入 → UiPaint + 本帧槽 buffer 句柄（record_frame 的 UI 段
    // 接画）。资源束缺席 = overlay 未装，整段跳过（收账日志在 paint_overlay 内）。
    // overlay 束按值消费（take 后本帧留 None；SystemParam 每帧重建，无残留）。
    let ui = match overlay {
        Some(mut ui_data) => {
            let ui_buffers = ui_data.ring.buffers(frames.current_index());
            let paint = paint_overlay(
                ctx,
                &ui_data.egui,
                &mut ui_data.frame,
                &ui_data.mirror,
                &mut ui_data.gpu,
                &mut ui_data.ring,
                &mut tables,
                uploader,
                frames.current_index(),
                swapchain.extent,
                &mut paint_log,
            )
            .or_fatal("overlay 绘制半边失败，渲染链无法继续")?;
            paint.map(|p| (p, ui_buffers))
        }
        None => None,
    };

    // 5) 录制 + 提交。此刻 image_available 已被 present engine 置位、image 已到手
    //——从这里起任何失败都让同步状态无法原样恢复（signal 无人等 / fence 已 reset
    // 无人 signal），按 Tier② 冒泡退出（不得 warn 后继续——那是"等无人 signal
    // 的 fence"死锁面）。上传票据在 GPU 侧等待（ALL_COMMANDS），CPU 不阻塞。
    let image = swapchain.images[index as usize];
    let view = swapchain.views[index as usize];
    // 透明层录制输入（8.1.3）：在场行 > 0 才录三 dispatch 与透明绘制；
    // collect 可失败（透明行容量上界，显式报错）——组装提到 FrameDraw 之外。
    let transparent_frame = if indirect_count > 0 {
        let slot_now = frames.current_index();
        Some(TransparentFrame {
            cluster_build: Some(transparent_passes.build(cluster_targets.grid())),
            cluster_set: cluster_targets.frame_set(slot_now),
            collect: Some(
                transparent_passes
                    .collect(indirect_count)
                    .or_fatal("透明行收集 dispatch 输入组装失败，渲染链无法继续")?,
            ),
            gather: Some(transparent_passes.gather(indirect_count)),
            sort: (prev_transparent_rows > 0).then(|| transparent_passes.sort_plan()),
            set2: instance_tables.frame_set2(slot_now),
            pipeline: transparent_pipeline.pipeline(),
            layout: transparent_pipeline.layout(),
            t_commands: cluster_targets.t_command_buffer(slot_now),
            t_keys: cluster_targets.t_key_buffer(slot_now),
            command_count: indirect_count,
        })
    } else {
        None
    };
    // shadow depth pass 输入（8.3）：在场行 > 0 且开关开才录四级联绘制；
    // 关 = 画面与 8.2 基线一致（composite 侧 shader 旁路 shadow 采样）
    let shadow_frame_input = if shadow_settings.enabled && indirect_count > 0 {
        Some(ShadowWriteDraw {
            pipeline: shadow_pipeline.pipeline(),
            layout: shadow_pipeline.layout(),
            set2: instance_tables.frame_set2(frames.current_index()),
            shadow_set: shadow_targets.frame_set(frames.current_index()),
            views: [
                shadow_targets.map_view(0),
                shadow_targets.map_view(1),
                shadow_targets.map_view(2),
                shadow_targets.map_view(3),
            ],
            images: [
                shadow_targets.map_image(0),
                shadow_targets.map_image(1),
                shadow_targets.map_image(2),
                shadow_targets.map_image(3),
            ],
            commands: [
                shadow_targets.command(frames.current_index(), 0),
                shadow_targets.command(frames.current_index(), 1),
                shadow_targets.command(frames.current_index(), 2),
                shadow_targets.command(frames.current_index(), 3),
            ],
            command_count: indirect_count,
            vertex_buffer: pool.vertex_buffer().ok_or_else(|| {
                VulkanError::Upload("shadow 输入在场行非空但顶点池缺席——状态矛盾".into())
            })?,
            index_buffer: pool.index_buffer().ok_or_else(|| {
                VulkanError::Upload("shadow 输入在场行非空但索引池缺席——状态矛盾".into())
            })?,
        })
    } else {
        None
    };
    // Bloom 链录制输入（8.4）：开关开才录九个全屏 pass；关 = None
    //（tonemap 侧 shader 不采样 bloom，up0 陈旧无消费）。
    let bloom_frame_input = if bloom_settings.enabled {
        Some(crate::render::BloomFrameDraw {
            prefilter_pipeline: bloom_pipelines.prefilter_pipeline(),
            prefilter_layout: bloom_pipelines.prefilter_layout(),
            down_pipeline: bloom_pipelines.down_pipeline(),
            down_layout: bloom_pipelines.down_layout(),
            up_pipeline: bloom_pipelines.up_pipeline(),
            up_layout: bloom_pipelines.up_layout(),
            gbuffer_set: gbuffer.sampled_set(frames.current_index()),
            down_sets: [
                bloom_targets.set_down(1, frames.current_index()),
                bloom_targets.set_down(2, frames.current_index()),
                bloom_targets.set_down(3, frames.current_index()),
                bloom_targets.set_down(4, frames.current_index()),
            ],
            up_sets: [
                bloom_targets.set_up(0, frames.current_index()),
                bloom_targets.set_up(1, frames.current_index()),
                bloom_targets.set_up(2, frames.current_index()),
                bloom_targets.set_up(3, frames.current_index()),
            ],
            images_down: [
                bloom_targets.down_image(0),
                bloom_targets.down_image(1),
                bloom_targets.down_image(2),
                bloom_targets.down_image(3),
                bloom_targets.down_image(4),
            ],
            images_up: [
                bloom_targets.up_image(0),
                bloom_targets.up_image(1),
                bloom_targets.up_image(2),
                bloom_targets.up_image(3),
            ],
            views_down: [
                bloom_targets.down_view(0),
                bloom_targets.down_view(1),
                bloom_targets.down_view(2),
                bloom_targets.down_view(3),
                bloom_targets.down_view(4),
            ],
            views_up: [
                bloom_targets.up_view(0),
                bloom_targets.up_view(1),
                bloom_targets.up_view(2),
                bloom_targets.up_view(3),
            ],
            extents: [
                bloom_targets.level_extent(0),
                bloom_targets.level_extent(1),
                bloom_targets.level_extent(2),
                bloom_targets.level_extent(3),
                bloom_targets.level_extent(4),
            ],
        })
    } else {
        None
    };
    // SMAA 录制输入（8.5）：edge/blend 开 = Some；neighborhood 恒 Some。
    let smaa_edge_blend_input = if smaa_settings.enabled {
        Some(SmaaEdgeBlendDraw {
            edge_pipeline: smaa_pipelines.edge_pipeline(),
            edge_layout: smaa_pipelines.edge_layout(),
            blend_pipeline: smaa_pipelines.blend_pipeline(),
            blend_layout: smaa_pipelines.blend_layout(),
            edge_set: smaa_targets.set_edge(frames.current_index()),
            blend_set: smaa_targets.set_blend(frames.current_index()),
            edges_view: smaa_targets.edges_view(frames.current_index()),
            edges_image: smaa_targets.edges_image(frames.current_index()),
            blend_view: smaa_targets.blend_view(frames.current_index()),
            blend_image: smaa_targets.blend_image(frames.current_index()),
            extent: smaa_targets.ldr_extent(),
        })
    } else {
        None
    };
    let smaa_nb_input = SmaaNbDraw {
        nb_pipeline: smaa_pipelines.neighborhood_pipeline(),
        nb_layout: smaa_pipelines.neighborhood_layout(),
        nb_set: smaa_targets.set_nb(frames.current_index()),
        ldr_view: smaa_targets.ldr_view(frames.current_index()),
        ldr_image: smaa_targets.ldr_image(frames.current_index()),
        ldr_extent: smaa_targets.ldr_extent(),
    };
    let frame_draw = FrameDraw {
        pipeline: pipeline.pipeline(),
        layout: pipeline.layout(),
        set0: tables.resident_set(),
        set1: tables.frame_set(frames.current_index()),
        set2: instance_tables.frame_set2(frames.current_index()),
        vertex_buffer: pool.vertex_buffer(),
        index_buffer: pool.index_buffer(),
        // 命令环句柄在 count = 0 时不被读（record_frame 的场景绘制整段跳过）
        indirect_count,
        indirect_buffer: instance_tables.indirect_buffer(frames.current_index()),
        gbuffer,
        // FrameDraw 借 GBufferTargets 引用（record_frame 消费附件视图与采样集）
        composite_pipeline: composite_pipeline.pipeline(),
        composite_layout: composite_pipeline.layout(),
        // 8.2.3：合成光照累积的 cluster 集（与透明层同一 layout 对象）
        cluster_set: cluster_targets.frame_set(slot_now),
        tonemap_pipeline: tonemap_pipeline.pipeline(),
        tonemap_layout: tonemap_pipeline.layout(),
        bloom_params_set: bloom_targets.params_set(frames.current_index()),
        bloom: bloom_frame_input,
        smaa_edge_blend: smaa_edge_blend_input,
        smaa_nb: smaa_nb_input,
        overlay_pipeline: overlay_pipeline.pipeline(),
        overlay_layout: overlay_pipeline.layout(),
        ui: ui.as_ref().map(|(paint, _)| paint),
        // ui 为 None 时这两个句柄不被读（record_frame 的 UI 段整段跳过）
        ui_buffers: ui
            .as_ref()
            .map(|(_, buffers)| *buffers)
            .unwrap_or((vk::Buffer::null(), vk::Buffer::null())),
        wait_ticket: uploader.last_issued_ticket(),
        ticket_semaphore: uploader.ticket_semaphore(),
        clear: CLEAR_COLOR,
        cull: cull_dispatch,
        shadow_build: shadow_build_dispatch,
        shadow: shadow_frame_input,
        shadow_set: shadow_targets.frame_set(frames.current_index()),
        shadow_images: [
            shadow_targets.map_image(0),
            shadow_targets.map_image(1),
            shadow_targets.map_image(2),
            shadow_targets.map_image(3),
        ],
        indirect: indirect_dispatch,
        occlusion: occl_dispatch,
        lod: lod_dispatch,
        // 透明层（8.1.3）：在场行 > 0 才录三 dispatch 与透明绘制；cluster
        // 构建与场景规模无关（按 cluster 网格），在场为 0 时整段跳过。
        transparent: transparent_frame,
        // 归约链恒录（见 FrameDraw.hiz），warmup 走账与录制一一对应
        hiz: Some(&*hiz),
        timer: timer.enabled.then_some(&*timer),
    };
    let record_clock = std::time::Instant::now();
    frames
        .record_frame(
            ctx,
            swapchain.render_finished[index as usize],
            image,
            view,
            swapchain.extent,
            &frame_draw,
        )
        .or_fatal("录制/提交失败，渲染链无法继续")?;
    let record_submit_ms = record_clock.elapsed().as_secs_f32() * 1000.0;

    // 6) present：把画好的 image 交给 present engine，等它的信号量按 acquire 的
    // image index 取（不按帧槽轮转）。SUBOPTIMAL/OUT_OF_DATE 都只是标记 pending
    //（同 acquire 的次优），重建交给下个帧首的闸门③；其余真失败不进通道也不
    // 提前返回——advance 必须照常推进保持帧槽账本一致，就地 error 留档 + 置位
    // 看门，退出由下一帧 Update 的 fatal_gate 生效（判定线：不提前 return 的
    // 失败不冒泡，就地处理）。
    let present_clock = std::time::Instant::now();
    if let Err(e) = swapchain.present(ctx.queue, swapchain.render_finished[index as usize], index) {
        match e {
            VulkanError::SwapchainOutOfDate => gate.pending = true,
            e => {
                error!("present 失败，渲染链无法继续: {e}");
                request_graceful_exit();
            }
        }
    }
    let present_ms = present_clock.elapsed().as_secs_f32() * 1000.0;

    // —— 帧样本入账（5.5）：CPU 分段 + GPU 区间 + 规模读数，环形窗口逐帧
    // push；只进计量（诊断性回读边界照 5.2 拍板）。让路帧（最小化/acquire
    // 过时）不经过此处，不进样本。
    let cpu_total_ms = wait_slot_ms + acquire_ms + assemble_ms + rings_ms + record_submit_ms + present_ms;
    perf.push(FrameSample {
        frame_wall_ms,
        wait_slot_ms,
        acquire_ms,
        assemble_ms,
        rings_ms,
        record_submit_ms,
        present_ms,
        cpu_total_ms,
        gpu_compute_ms: gpu_sections[0],
        gpu_shadow_ms: gpu_sections[1],
        gpu_gbuffer_ms: gpu_sections[2],
        gpu_lighting_ms: gpu_sections[3],
        gpu_hiz_ms: gpu_sections[4],
        gpu_present_ms: gpu_sections[5],
        gpu_total_ms: gpu_sections[7],
        instance_rows: indirect_count,
        visible_rows: visible_among_live,
    });

    // 归约链已录（见 FrameDraw.hiz），warmup 走账与录制一一对应
    hiz.note_reduced();
    frames.advance();
    Ok(())
}

/// 清屏色：固定深蓝纯色。线性域值——SRGB view 在 ROP 写出时才做编码
///（frames.rs），调色时按线性直觉给值，不要拿 sRGB 十六进制直接换算。
const CLEAR_COLOR: [f32; 4] = [0.02, 0.06, 0.14, 1.0];

/// [f32; 4] 的收账日志格式（灯光 UBO 的 rgb 分量可读性）。
fn fmt_vec4(v: [f32; 4]) -> String {
    format!("({:.1}, {:.1}, {:.1}, {:.1})", v[0], v[1], v[2], v[3])
}

#[cfg(test)]
mod tests {
    use super::OcclusionGate;
    use bevy::math::{Quat, Vec3};

    /// fallback 判据的阈值语义（判定线 2 的显式 fallback 判据，钉死）：比较
    /// "本帧 vs 两帧前"（与遮挡读数滞后同窗——金字塔正是两帧前的深度），
    /// 阈值作用于两帧累计量：历史不足 = 未就绪、静止不触发、累计微动/单帧
    /// 大跳超阈（位移 0.5 / 旋转 15°，钉明见 occlusion 模块注释）都触发。
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
        // 连续微动两帧窗口累计 0.6 > 0.5：触发——滞后窗语义下金字塔内容
        // 已落后累计量，让路是诚实行为（不是单帧阈值）
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
