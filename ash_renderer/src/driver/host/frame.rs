//! ash 帧循环：一次 update = 一帧——resize 闸门 → 等帧槽位空出 → acquire →
//! 组装 DrawList → 帧槽 UBO 与环写入 → compute 段 dispatch 与诊断回读 →
//! 录制（清屏+绘制）并提交 → present。
//!
//! 资源内聚在各模块：本模块只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
//! 系统注册与禁渲染补位在 [`super::host`]（AshHostPlugin），初始化与拆除链在
//! [`super::init`]。
//!
//! 住址：`Last`，与采集同帧闭环——Update 变更 → PostUpdate 传播+采集
//!（`scene::ledger` 账本对账）→ Last 消费账本：DrawList 槽位枚举与双环写入
//!（[`super::drawlist`]，逐对象参数在 GPU 常驻表）→ 录制提交。
//! GPU 执行异步——上传票据在 GPU 侧等待，CPU 不阻塞；材质按不透明调试
//! 策略覆盖（详见 draw_frame 收账日志）。
//!
//! 帧槽：`frames.current_index()` 只在系统末尾的 `advance` 处推进，draw_frame 在
//! `wait_for_slot` 后取一次为 `slot`，本帧全部帧槽环/UBO/描述符集按它定位。
//!
//! 帧内错误走统一错误通道（[`crate::common::error::tier_handler`]）：错误点一行
//! `or_warn/or_fatal`（挂 context 负载 + 标 Severity）后 `?` 直冒泡，日志在 handler
//! 统一打、退出裁决在看门 system——Tier① Warning 本帧让路（下帧自愈）；Tier②
//! 优雅退出。两处保持显式分支、不走 `?`：OUT_OF_DATE 是"重试"不是失败（控制流）；
//! present 真失败先暂存，advance 照常跑保持帧槽账本一致后再冒泡。
//! Tier② 的红线：acquire 成功之后的 Vulkan 真失败必须优雅退出——那时同步状态
//! 已无法恢复，warn 后继续会等一个永远无人 signal 的 fence，直接死锁。

use std::time::Instant;

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
        conservative_world_aabb, frustum_planes, pack_frame_uniforms, AcquireOutcome,
        BindlessTextureTable, Context, CullDiagnostics, CullDraw, CullPass, CullSettings, FrameDraw,
        FramePool, FrameSample, FrameUniformTable, FrameUniformsData, GpuBuffer, GBufferPipeline, GpuTimer,
        HizPyramid, ImageCache, IndirectDraw, IndirectPass, IndirectStats, InstanceTables,
        LOD_LEVEL_SIZE, LodDiagnostics, LodDraw, LodPass, LodSettings, MAX_LOD_LEVELS, MeshPool,
        OcclusionDiagnostics, OcclusionDraw, OcclusionFallback, OcclusionPass,
        OcclusionSettings, OverlayPipeline, PerfLedger, RenderMode, Swapchain, Uploader,
        GAP_MARK_SIZE, MAX_FRAMES_IN_FLIGHT, MARK_SIZE,
    },
};

use super::drawlist::{self, DrawabilitySources, ParityDirty};
use crate::render::{
    cascade_matrices, pack_cluster_uniforms, pack_shadow_uniforms, ClusterBuildPass, LightClusters,
    BloomFrameDraw, BloomPipelines, BloomSettings, BloomTargets, CompositePipeline,
    GBufferTargets, LightTable,
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
    PresentTonemapPipeline, ShadowBuildPass,
    ShadowCascadeInput, ShadowCascades, ShadowDiagnostics, ShadowPipeline, ShadowSettings,
    ShadowTargets,
    ShadowUniformsData, ShadowWriteDraw, TransparentFrame, TransparentPasses,
    TransparentPipeline, TransparentTargets,
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

impl ResizeGate {
    /// 闸门①②：吸收本帧 resize 消息，返回"本帧整帧让路"（最小化中）。
    ///
    /// 一次 update 可能积压多条消息（拖拽 125Hz 输入 vs 60fps 帧），逐条刷状态，
    /// 最新一条定生死：非零尺寸 = 正常/恢复，(0,0) = 最小化。最小化时 surface
    /// 没有 presentable 尺寸，任何 acquire/present 都可能无限阻塞——调用方直接
    /// 返回、不碰 Vulkan，runner 继续泵消息，还原点击才能被处理。
    fn yield_minimized(&mut self, resized: &mut MessageReader<WindowResized>) -> bool {
        for msg in resized.read() {
            if msg.width == 0.0 || msg.height == 0.0 {
                self.minimized = true;
            } else {
                self.minimized = false;
                self.pending = true;
            }
        }
        if self.minimized {
            if !self.logged_minimized {
                self.logged_minimized = true;
                info!("窗口最小化：帧循环整帧让路（不碰 swapchain），消息泵保持存活");
            }
            return true;
        }
        if self.logged_minimized {
            self.logged_minimized = false;
            info!("窗口脱离最小化：闸门重开");
        }
        false
    }
}

/// 遮挡 fallback 的相机历史（`draw_frame` 的 `Local`，跨帧保持）：跳变判定
/// 用"当前 vs 两帧前"——遮挡读的金字塔正是两帧前的深度（同帧槽 fence 纪律），
/// 对两帧前的机位判稳语义才对齐（阈值钉明见 occlusion 模块注释：位移 0.5 /
/// 旋转 15°，连续缓慢环绕不触发）。
#[derive(Default)]
pub(crate) struct OcclusionGate {
    prev: Option<(Vec3, Quat)>,
    prev_prev: Option<(Vec3, Quat)>,
}

impl OcclusionGate {
    /// 推入本帧机位并返回"相对两帧前是否跳变"（历史不足 = 未就绪 = 视同跳变）。
    fn push(&mut self, now: Option<(Vec3, Quat)>) -> bool {
        let jump = match (self.prev_prev, now) {
            (Some((p0, r0)), Some((p1, r1))) => {
                (p1 - p0).length() > 0.5 || Quat::angle_between(r0, r1) > 15f32.to_radians()
            }
            _ => true, // 机位历史不足（启动/相机缺席帧）= 未就绪
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
    /// GPU 缺口快照：帧起在 fence 纪律内读缺口标记环的采样落点——驻留管理器
    /// tick（Last，帧提交后）与 BRP census 经此消费，不直读环。
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

/// 延迟光照与后处理：G-buffer 附件 + HDR 中间目标（闸门③随 swapchain 重建）、
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

/// 帧循环日志状态（跨帧 `Local`）：缺资源/相机告警去重 + 一次性收账。
#[derive(Default)]
pub(crate) struct FrameLogs {
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
    /// 各 compute 段首个 dispatch 帧收账一次（零 VUID 由验证层执法）。
    cull_first_dispatch: OnceLog,
    occlusion_first_dispatch: OnceLog,
    lod_first_dispatch: OnceLog,
    indirect_first_submit: OnceLog,
}

/// ash 帧循环系统（注册与排序在 [`super::host`]）：resize 闸门 → 等帧槽位 →
/// acquire → 组装 DrawList → 帧槽 UBO 与环写入 → compute 段 → overlay 绘制
/// 半边 → 录制提交 → present。
///
/// 失败不设壳：错误点 `or_warn/or_fatal` 挂负载标级后 `?` 直冒泡——Err 上交统一
/// 错误通道（[`crate::common::error::tier_handler`] 分流：Warning 本帧让路，其余
/// 置位看门优雅退出），日志在 handler 统一打，本系统只管控制流。
#[system]
pub(crate) fn draw_frame(
    mut input: FrameInput,
    _main_thread: NonSendMarker,
) -> Result<(), BevyError> {
    // overlay 束按值消费（take 后本帧留 None；SystemParam 每帧重建，无残留）
    let overlay = input.overlay.take();
    let FrameInput {
        ref mut core,
        ref mut scene,
        ref mut cull,
        ref mut indirect,
        ref mut occlusion,
        ref mut lod,
        ref mut shadow,
        ref mut post,
        ref mut cluster,
        ref mut transparent,
        ref mut gap_snapshot,
        ref mut gate,
        ref mut logs,
        ref mut occlusion_gate,
        ref mut paint_log,
        ..
    } = input;
    if gate.yield_minimized(&mut core.resized) {
        return Ok(());
    }
    // pending 保持置位、全部重建成功才清：重建失败的帧在 or_warn/or_fatal 处
    // 让路或退出，下个帧首原样重试
    if gate.pending {
        rebuild_size_dependent(core, &mut occlusion.hiz, post, cluster)?;
        gate.pending = false;
    }

    // —— 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在提交前一刻）。
    // 失败 = 等待/重置层面坏掉（设备丢失、内存枯竭），继续帧循环只会撞上未知的
    // fence 状态——Tier②
    let frame_wall_ms = core.perf.mark_frame_start();
    let frame_clock = Instant::now();
    core.frames
        .wait_for_slot()
        .or_fatal("帧槽等待/重置失败，渲染链无法继续")?;
    let wait_slot_ms = ms_since(frame_clock);
    let slot = core.frames.current_index();
    // —— GPU 帧计时取回：本槽 fence 已等完 = 上轮提交执行完毕，timestamp 可读。
    // 混合口径如实标注：GPU 区间是本槽上轮（两帧前）的提交，稳态下与当前帧
    // 同构；停用时恒 0。
    let gpu_sections = core
        .timer
        .fetch(slot)
        .or_fatal("GPU 帧计时取回失败，渲染链无法继续")?
        .unwrap_or([0.0; 8]);

    // —— acquire：拿到一张可画的 image；OUT_OF_DATE = 这个 swapchain 已不可用
    //（最小化/恢复/独占模式切换等），必须立即重建——等不得下一帧
    let acquire_clock = Instant::now();
    let index = match core.swapchain.acquire(core.frames.current().image_available) {
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
            core.swapchain.rebuild(&core.ctx).or_warn("acquire 过时后重建失败")?;
            gate.pending = true;
            return Ok(());
        }
        Err(e) => return Err(fatal("acquire 失败，渲染链无法继续", e)),
    };
    let acquire_ms = ms_since(acquire_clock);

    // —— 组装 DrawList（增量维护）：账本组装缓存消费重估集（变更点 = 行增删/
    // 换绑/置脏/槽位交接/驻留出账的各钩子），行宇宙稳定帧零走访。可绘判定：
    // 实例槽缺/mesh 未驻留/材质缺席 = 暂缓（Tier① 自愈）。暂缓行数 = 行总数 −
    // 可绘数（缓存维护后恒成立）。
    let sources = DrawabilitySources {
        pool: &scene.pool,
        std_materials: &scene.std_materials,
        lod_groups: &scene.lod_groups,
        floor_anchor: scene.residency_settings.floor_anchor,
    };
    let assemble_clock = Instant::now();
    drawlist::assemble_update(&mut scene.ledger, sources);
    let assemble_ms = ms_since(assemble_clock);
    // 对照模式：BRP 开关开启时逐帧跑全量参照走访与增量缓存逐字段比对——只进
    // 计量，不驱动绘制。
    if scene.verify.enabled {
        let pending = scene.ledger.assembly_pending();
        drawlist::verify_assembly(&scene.ledger, sources, pending, &mut scene.verify);
    }
    // —— 奇偶集先于视图取走（写侧的账本可变消费点收口），此后 assembly
    // 视图对账本共享借用，环写入与诊断读数全程零可变访问。
    let dirty = ParityDirty::take(&mut scene.ledger, slot);
    let ledger: &InstanceLedger = &scene.ledger;
    let assembly = ledger.assembly_view();
    let pending_rows = ledger.len() - assembly.slots.len();
    if pending_rows > 0 {
        if !logs.warned_pending {
            logs.warned_pending = true;
            info!(
                "DrawList 暂缓 {}/{} 行（实例槽/mesh/材质未就绪，上传链自愈后消失）",
                pending_rows,
                ledger.len(),
            );
        }
    } else {
        logs.warned_pending = false;
    }
    if !ledger.is_empty() && pending_rows == 0 {
        logs.override_logged.log(|| info!(
            "材质调试覆盖：{} 个 primitive 全部按不透明绘制——blend 关闭（alpha<1 的 {} 行照常画出，glTF BLEND 镜片在此列）；材质参数/贴图槽走材质环（每帧从容器现读）；着色模式按 ASH_RENDER_MODE（三态，见灯光收账日志）",
            ledger.len(),
            assembly.alpha_override,
        ));
    }

    // —— 帧槽 UBO 与环写入：复用安全 = wait_for_slot 的 fence——写发生在本槽上一轮
    // 使用完成之后（帧槽轮转只是定位，安全证据是 fence）。
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
    write_frame_ubo(&mut core.frame_uniform_table, slot, &view, &light, render_mode)?;
    let scene_aabb = scene_world_aabb(ledger, &scene.pool);
    write_shadow_ubo(shadow, slot, &view, &light, scene_aabb)?;
    write_post_ubos(post, slot)?;
    write_cluster_slot(cluster, slot, &view, core.swapchain.extent)?;
    write_transparent_slot(transparent, slot)?;
    // 双环写入：模型环增量 + 材质环全量 + 在场环 + LOD 表增量（hazard 纪律表见
    // instance_table 模块文档）。
    let rings_clock = Instant::now();
    drawlist::write_rings(
        &mut scene.instance_tables,
        ledger,
        sources,
        &scene.image_cache,
        &dirty,
        &assembly,
        slot,
    )
    .or_fatal("模型/材质/在场/LOD 环写失败，渲染链无法继续")?;
    let rings_ms = ms_since(rings_clock);

    // —— compute 段：各半边按开关决定 dispatch 或 host 填充（数据层 A/B），诊断
    // 读数在写入路径之后取，标记环读写都在 wait_for_slot 的 fence 纪律内。诊断
    // 只进计量（任何系统不得读它决定画什么）。
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

    // —— overlay 绘制半边：图集整传（可能多出一张 transfer 票据——本帧图形提交的
    // wait_ticket 在此后快照，flush_uploads 批与图集批都被等待）→ 镶嵌 → 顶点环
    // 写入 → UiPaint + 本帧槽 buffer 句柄（record_frame 的 UI 段接画）。资源束
    // 缺席 = overlay 未装，整段跳过（收账日志在 paint_overlay 内）。
    let ui = match overlay {
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
            )
            .or_fatal("overlay 绘制半边失败，渲染链无法继续")?;
            paint.map(|p| (p, ui_buffers))
        }
        None => None,
    };

    // —— 录制 + 提交。此刻 image_available 已被 present engine 置位、image 已到手
    //——从这里起任何失败都让同步状态无法原样恢复（signal 无人等 / fence 已 reset
    // 无人 signal），按 Tier② 冒泡退出（不得 warn 后继续——那是"等无人 signal
    // 的 fence"死锁面）。上传票据在 GPU 侧等待（ALL_COMMANDS），CPU 不阻塞。
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
        // 在场行 > 0 才录透明三 dispatch 与透明绘制；cluster 构建与场景规模无关
        //（按 cluster 网格），在场为 0 时整段跳过。
        transparent: transparent_frame,
        // 归约链恒录（见 FrameDraw.hiz），warmup 走账与录制一一对应
        hiz: Some(&*occlusion.hiz),
        timer: core.timer.enabled.then_some(&*core.timer),
    };
    let record_clock = Instant::now();
    core.frames
        .record_frame(
            &core.ctx,
            core.swapchain.render_finished[index as usize],
            image,
            target_view,
            extent,
            &frame_draw,
        )
        .or_fatal("录制/提交失败，渲染链无法继续")?;
    let record_submit_ms = ms_since(record_clock);

    // —— present：把画好的 image 交给 present engine，等它的信号量按 acquire 的
    // image index 取（不按帧槽轮转）。SUBOPTIMAL/OUT_OF_DATE 都只是标记 pending
    //（同 acquire 的次优），重建交给下个帧首的闸门③；其余真失败先暂存——提交
    // 已发生，advance 必须照常推进保持帧槽账本一致，收尾后在系统末尾 Tier② 冒泡。
    let present_clock = Instant::now();
    let present_failure = match core.swapchain.present(
        core.ctx.queue,
        core.swapchain.render_finished[index as usize],
        index,
    ) {
        Ok(_) => None,
        Err(VulkanError::SwapchainOutOfDate) => {
            gate.pending = true;
            None
        }
        Err(e) => Some(e),
    };
    let present_ms = ms_since(present_clock);

    // —— 帧样本入账：CPU 分段 + GPU 区间 + 规模读数，环形窗口逐帧 push；只进计量。
    // 让路帧（最小化/acquire 过时）不经过此处，不进样本。
    let cpu_total_ms = wait_slot_ms + acquire_ms + assemble_ms + rings_ms + record_submit_ms + present_ms;
    core.perf.push(FrameSample {
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
    occlusion.hiz.note_reduced();
    core.frames.advance();
    match present_failure {
        Some(e) => Err(fatal("present 失败，渲染链无法继续", e)),
        None => Ok(()),
    }
}

/// 闸门③：尺寸变化 → 帧首按需重建，时机在 acquire 之前。rebuild 幂等（尺寸
/// 没变就空手而归），拖拽中每步一建、帧循环不断流——acquire 永远落在新
/// swapchain 上。其余尺寸相关资源随其后按新 extent 重建（同样幂等；各自带
/// device_wait_idle，在飞旧资源不可能被引用）。swapchain 重建失败 Tier①
/// 让路下帧重试，其余失败 Tier②。
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
    // Hi-Z 金字塔随深度附件同批重建（重建后 warmup 重置——遮挡测试 fallback
    // 到金字塔重新就绪）
    hiz.rebuild(ctx, extent, &per_slot(|s| core.frames.depth_image(s)))
        .or_fatal("Hi-Z 金字塔重建失败，渲染链无法继续")?;
    post.bloom
        .rebuild(ctx, extent)
        .or_fatal("Bloom 链重建失败，渲染链无法继续")?;
    // 深度视图已按新尺寸重建——G-buffer 采样集 b3 的重建依赖
    post.gbuffer
        .rebuild(ctx, extent, &per_slot(|s| core.frames.depth_view(s)))
        .or_fatal("G-buffer 附件重建失败，渲染链无法继续")?;
    // 光源常驻表与尺寸无关不重建，只把它的 buffer 重新绑进新描述符集
    cluster
        .clusters
        .rebuild(ctx, extent, &per_slot(|s| cluster.lights.ring_buffer(s)))
        .or_fatal("光源分簇 cluster 表重建失败，渲染链无法继续")?;
    // SMAA 三目标（LDR/edges/blend；描述符集随 build_descriptors 重写）——resize
    // 后 tonemap/edge/blend 的 renderArea 与附件尺寸必须同帧对齐
    post.smaa
        .rebuild(ctx, extent)
        .or_fatal("SMAA 三目标重建失败，渲染链无法继续")?;
    Ok(())
}

/// 逐帧槽收集（重建入参：深度 image/view、光源环 buffer）。
fn per_slot<T>(f: impl FnMut(usize) -> T) -> Vec<T> {
    (0..MAX_FRAMES_IN_FLIGHT).map(f).collect()
}

/// 自 `start` 起的毫秒数（帧样本 CPU 分段）。
fn ms_since(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
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
    /// 首个取到灯光的帧收账一次。
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
            // 无灯：直射项归零，方向留合法占位（共享常量见 frame_uniform_table.rs）
            None => (crate::render::PLACEHOLDER_DIR_TO_LIGHT, [0.0; 4]),
        };
        if light_count != 1 && !ledger_empty {
            logs.warned_light_count.log(|| {
                warn!("方向光 {light_count} 盏（取第一盏/无灯直射项归零），多灯支持未实现")
            });
        }
        let ambient_color = match view.ambient_override {
            Some(a) => LinearRgba::from(a.color).to_f32_array().map(|c| c * a.brightness),
            None => match global_ambient {
                Some(global) => {
                    LinearRgba::from(global.color).to_f32_array().map(|c| c * global.brightness)
                }
                None => {
                    logs.warned_no_ambient.log(|| {
                        warn!("环境光缺席：GlobalAmbientLight 资源与相机 AmbientLight 均无，环境项归零")
                    });
                    [0.0; 4]
                }
            },
        };
        if light_count > 0 {
            logs.lights_logged.log(|| info!(
                "灯光 UBO 收账：方向光 {light_count} 盏（dir_to_light = back()，light = {}）；\
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
fn write_frame_ubo(
    frame_uniform_table: &mut FrameUniformTable,
    slot: usize,
    view: &FrameView,
    light: &FrameLight,
    render_mode: RenderMode,
) -> Result<(), BevyError> {
    frame_uniform_table
        .ubo(slot)
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

/// 场景世界 AABB 并集（级联 far 外扩 + 深度盒向太阳侧延伸到场景顶的输入）：
/// 账本行 × 驻留局部包围盒 → 世界保守盒（cull_stats CPU 参照同款走访）。
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

/// shadow UBO（帧槽环与主 UBO 同一 fence 证据）：级联矩阵（log 分割拟合）+
/// 四×六平面（cull 内联标记与 shadow_stats CPU 参照同源）+ 分割带/相机基
///（composite cascade 选择）。相机缺席帧 = identity 矩阵（与帧 UBO 兜底同口径）。
fn write_shadow_ubo(
    shadow: &mut ShadowInput,
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
        enabled: f32::from(shadow.settings.enabled),
        texel_world: fit.texel_world,
        inv_span: fit.inv_span,
    };
    shadow
        .targets
        .ubo(slot)
        .write(0, &pack_shadow_uniforms(&uniform))
        .or_fatal("shadow UBO 写失败，渲染链无法继续")
}

/// 后处理帧槽 UBO（同一 fence 证据）：Bloom 五标量（BRP bloom_set 直达）+
/// SmaaInfo（rt_metrics + 开关）。
fn write_post_ubos(post: &mut PostInput, slot: usize) -> Result<(), BevyError> {
    let bloom = &post.bloom_settings;
    post.bloom
        .ubo(slot)
        .write(
            0,
            &crate::render::pack_bloom_params(&crate::render::BloomParamsData {
                strength: bloom.strength,
                enabled: f32::from(bloom.enabled),
                tonemap: f32::from(bloom.tonemap),
                curve: bloom.curve.as_f32(),
                gain: bloom.gain,
            }),
        )
        .or_fatal("Bloom 参数 UBO 写失败，渲染链无法继续")?;
    let ldr = post.smaa.ldr_extent();
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
            f32::from(post.smaa_settings.enabled),
            0.0,
            0.0,
            0.0,
        ],
    );
    post.smaa
        .info_mut(slot)
        .write(0, &smaa_info)
        .or_fatal("SmaaInfo UBO 写失败，渲染链无法继续")
}

/// 写本帧槽的 cluster 均匀数据和光源常驻表脏行。调用时该槽 fence 已等过。
fn write_cluster_slot(
    cluster: &mut ClusterInput,
    slot: usize,
    view: &FrameView,
    extent: vk::Extent2D,
) -> Result<(), BevyError> {
    let cluster_uniform = pack_cluster_uniforms(
        &view.view_proj,
        &view.inv_view_proj,
        extent,
        cluster.clusters.grid(),
        cluster.lights.light_count(),
    );
    cluster
        .clusters
        .uniform(slot)
        .write(0, &cluster_uniform)
        .or_fatal("cluster 均匀数据写失败，渲染链无法继续")?;
    // 只写脏行；一行要在两个帧槽都写过才算清
    cluster
        .lights
        .write_dirty(slot)
        .or_fatal("光源常驻表脏行写失败，渲染链无法继续")?;
    Ok(())
}

/// 本帧槽透明行数清零，collect 在 GPU 上从 0 原子累加。
fn write_transparent_slot(transparent: &mut TransparentInput, slot: usize) -> Result<(), BevyError> {
    transparent
        .targets
        .count(slot)
        .write(0, &0u32.to_le_bytes())
        .or_fatal("透明行计数清零失败，渲染链无法继续")?;
    Ok(())
}

/// 本帧 compute 段的共享输入。
struct PassFrame<'a> {
    slot: usize,
    /// 对象表已用槽上界（dispatch 线程数与标记环读写长度）。
    bound: u32,
    view_proj: Mat4,
    screen: [f32; 2],
    /// 在场行实例槽号（诊断读数逐行计数的行宇宙）。
    live: &'a [u32],
}

/// 视锥剔除半边：开关决定 compute dispatch 或 host 全可见填充。诊断读数：
/// 开 = 本帧槽两帧前 compute 的输出（最新确定值）；关 = host 全可见填充后直读
///（A/B 的 B 面基准；未 dispatch 时 build_indirect 读的就是这份填充，dispatch
/// 时 shader 同写 1）。返回 (dispatch, 在场行可见数)。
fn cull_half(
    pf: &PassFrame,
    cull: &mut CullInput,
    shadow_enabled: bool,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<(Option<CullDraw>, u32), BevyError> {
    let bound = pf.bound;
    // shadow 开启时剔除 dispatch 也承担 shadow 标记产出——即使主视锥 A/B 关，
    // dispatch 仍要跑（写 shadow 标记）；两侧全关才可跳过。主视图开关随 push
    // 进 shader：关 = marks 恒写 1，不覆盖下方 host 全可见填充的语义
    let dispatch = if (cull.settings.enabled || shadow_enabled) && bound > 0 {
        first.log(|| info!(
            "视锥剔除 dispatch 首跑（前置闸门④）：bound {bound}，组 {}，平面随 112B push 进 compute",
            bound.div_ceil(64),
        ));
        Some(cull.pass.draw(&frustum_planes(&pf.view_proj), bound, cull.settings.enabled))
    } else {
        None
    };
    if !cull.settings.enabled && bound > 0 {
        fill_u32(instances.mark_ring(pf.slot), bound as usize, 1)
            .or_fatal("剔除标记环全可见填充失败，渲染链无法继续")?;
    }
    let mut visible = 0u32;
    if bound > 0 {
        let marks = read_ring(instances.mark_ring(pf.slot), bound as usize * MARK_SIZE as usize)
            .or_fatal("剔除标记环读数失败，渲染链无法继续")?;
        visible = count_marked(&marks, pf.live);
        cull.diag.enabled = cull.settings.enabled;
        cull.diag.bound = bound;
        cull.diag.marks_frame = pf.slot;
        cull.diag.rows_compared = pf.live.len();
        cull.diag.visible_count = visible;
    }
    Ok((dispatch, visible))
}

/// shadow 标记半边：剔除 dispatch 跑了 → 标记 = compute 输出；没跑（shadow 关
/// 或空场）→ host 全可见填充（读数语义 = 剔除量 0；composite 侧 shadow 关 =
/// shader 旁路，标记内容无消费）。诊断逐 cascade 计数（陈旧 ≤2 帧与主标记同款）。
fn shadow_marks_half(pf: &PassFrame, shadow: &mut ShadowInput) -> Result<(), BevyError> {
    let bound = pf.bound;
    if !(shadow.settings.enabled && bound > 0) {
        fill_u32(shadow.targets.mark_mut(pf.slot), bound as usize * SHADOW_CASCADES, 1)
            .or_fatal("shadow 标记环全可见填充失败，渲染链无法继续")?;
    }
    if bound > 0 {
        let marks = read_ring(shadow.targets.mark_mut(pf.slot), bound as usize * SHADOW_CASCADES * 4)
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
    }
    Ok(())
}

/// 遮挡 dispatch 判据（单源：dispatch 与否和 fallback 原因同出一处）。
enum OcclusionVerdict {
    Dispatch,
    /// 跳过 dispatch；原因取首个命中（空场 = None：无可判行，不算 fallback）。
    Skip(Option<OcclusionFallback>),
}

impl OcclusionVerdict {
    /// fallback 判据逐项核对（occlusion 模块注释钉明的四条 + 空场）。
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

/// Hi-Z 遮挡半边：判据任一命中 → 本帧跳过 dispatch——b3 保持视锥纯结果、b6 host
/// 填全可见（读数语义 = 遮挡剔除量 0，A/B 的 B 面同口径）。跳变判定用"当前 vs
/// 两帧前"机位（[`OcclusionGate`]）。诊断读数 b6 = 本帧槽两帧前 compute 输出或
/// host 全可见填充。
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
                "Hi-Z 遮挡测试 dispatch 首跑（前置闸门复用视锥 compute 链）：bound {bound}，\
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

/// GPU LOD 选档半边：开关决定 compute dispatch 或 host 档位环填 0（选档每帧从
/// 当前相机现算，无 fallback 专段——档位环只是迟滞状态不是真值，lod 模块注释
/// 钉明）。诊断：档位分布逐在场行统计（b8 = 本帧槽两帧前 compute 输出或 host 填 0）。
fn lod_half(
    pf: &PassFrame,
    lod: &mut LodInput,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<Option<LodDraw>, BevyError> {
    let bound = pf.bound;
    let dispatch = if lod.settings.enabled && bound > 0 {
        first.log(|| info!(
            "GPU LOD 选档 dispatch 首跑（前置闸门复用视锥 compute 链）：bound {bound}，\
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

/// 缺口快照采样：帧起读缺口标记环（本帧槽 = 两帧前 compute 输出，fence 纪律同
/// 档位环诊断）——驻留管理器 tick 挂 Last 在帧提交之后，直读环会与在飞 compute
/// 写竞争，消费口统一走本快照（tick 与 BRP census）。
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
        // 缺席槽清零归 cull dispatch（bound 全覆盖）；本帧未 dispatch（剔除与
        // 阴影都关）时缺席槽会残留陈旧位图——host 补零（在场槽由本帧
        // build_indirect 照常写新鲜值）。补零必须在快照读数之后：先清会抹掉两帧前
        // build_indirect 写下的在场槽缺口，驻留管理器永远看不到缺口
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

/// indirect 命令组装半边：在场行数 = GPU 命令条数（CPU 在账信息，非可见集回读）；
/// count > 0 才有命令可组装（空场 = None，场景绘制整段跳过）。诊断读数（draw
/// 调用数/命令条数/instanceCount=0 条数）只进计量。
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
            "indirect 提交首跑（前置闸门②）：在场行 {count}，命令环 {}B/条，\
             图形侧一次 cmd_draw_indexed_indirect（图形 push range 归零，firstInstance 携带槽号）",
            crate::render::INDIRECT_COMMAND_SIZE,
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

/// host 侧整段填充 u32 环：`count` 个小端 `value`（A/B 的 B 面全可见/全最细档）。
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

/// [f32; 4] 的收账日志格式（灯光 UBO 的 rgb 分量可读性）。
fn fmt_vec4(v: [f32; 4]) -> String {
    format!("({:.1}, {:.1}, {:.1}, {:.1})", v[0], v[1], v[2], v[3])
}

#[cfg(test)]
mod tests {
    use super::OcclusionGate;
    use bevy::math::{Quat, Vec3};

    /// fallback 判据的阈值语义（显式 fallback 判据，钉死）：比较"本帧 vs 两帧前"
    ///（与遮挡读数滞后同窗——金字塔正是两帧前的深度），阈值作用于两帧累计量：
    /// 历史不足 = 未就绪、静止不触发、累计微动/单帧大跳超阈（位移 0.5 / 旋转
    /// 15°，钉明见 occlusion 模块注释）都触发。
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
