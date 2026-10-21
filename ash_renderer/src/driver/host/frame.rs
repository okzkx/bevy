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
    vulkan::{
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
        ref lod_groups,
        ref residency_settings,
        ref mut gap_snapshot,
        ref mut timer,
        ref mut perf,
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
        .unwrap_or([0.0; 4]);

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

    // 3) 组装 DrawList：槽位枚举（账本行 → 实例槽 + 池引脚；逐对象参数已进
    // GPU 常驻表，push 只剩 4B 实例槽号——见 drawlist 模块文档）。可绘判定
    // 与 4.x 语义一致：实例槽缺/mesh 未驻留/材质缺席 = 暂缓（Tier① 自愈）。
    let assemble_clock = std::time::Instant::now();
    let assembly =
        drawlist::assemble(ledger, pool, std_materials, lod_groups, residency_settings.floor_anchor);
    let assemble_ms = assemble_clock.elapsed().as_secs_f32() * 1000.0;
    if assembly.pending_rows > 0 {
        if !draw_state.warned_pending {
            draw_state.warned_pending = true;
            info!(
                "DrawList 暂缓 {}/{} 行（实例槽/mesh/材质未就绪，上传链自愈后消失）",
                assembly.pending_rows,
                ledger.len(),
            );
        }
    } else {
        draw_state.warned_pending = false;
    }
    if !ledger.is_empty() && assembly.pending_rows == 0 {
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
    let mut per_camera_ambient: Option<&AmbientLight> = None;
    let mut cam_now: Option<(Vec3, bevy::math::Quat)> = None;
    match cameras.single() {
        Ok((projection, global, ambient_override)) => {
            view_proj = projection.get_clip_from_view() * global.affine().inverse();
            per_camera_ambient = ambient_override;
            cam_now = Some((global.translation(), global.rotation()));
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
            crate::vulkan::PLACEHOLDER_DIR_TO_LIGHT,
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
                RenderMode::Lambert => "lambert",
                RenderMode::Unlit => "unlit",
                RenderMode::Normal => "normal",
            },
        ));
    }
    tables
        .frame_ubo(frames.current_index())
        .write(
            0,
            &pack_frame_uniforms(&FrameUniformsData {
                view_proj,
                dir_to_light,
                ambient_color,
                light_color,
                mode: render_mode.as_u32(),
            }),
        )
        .or_fatal("帧 UBO 写失败，渲染链无法继续")?;

    // 4a) 双环写入（wait_for_slot 之后，与帧 UBO 同一条 fence 证据）：模型环
    // 增量重写（本帧奇偶集的改动行）+ 材质环全量重写（可绘行的材质从容器
    // 现读）。见 instance_table 模块文档的 hazard 纪律表。
    let rings_clock = std::time::Instant::now();
    drawlist::write_rings(
        instance_tables,
        ledger,
        std_materials,
        image_cache,
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
    let cull_dispatch = if cull_settings.enabled && cull_bound > 0 {
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
        Some(crate::vulkan::OcclusionFallback::Disabled)
    } else if !cull_settings.enabled {
        Some(crate::vulkan::OcclusionFallback::CullOff)
    } else if cull_bound == 0 {
        None // 空场：无可判行，不算 fallback（画面语义 = 本来就无对象）
    } else if !hiz.ready() {
        Some(crate::vulkan::OcclusionFallback::PyramidWarmup)
    } else if camera_jumped {
        Some(crate::vulkan::OcclusionFallback::CameraJump)
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
        for slot in &assembly.slots {
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
                crate::vulkan::INDIRECT_COMMAND_SIZE,
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
    indirect_stats.pending_rows = assembly.pending_rows;

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
        indirect: indirect_dispatch,
        occlusion: occl_dispatch,
        lod: lod_dispatch,
        // 归约链恒录（深度每帧变，无隔帧空间；warmup 在 hiz 内走账——
        // note_reduced 与录制一一对应，让路帧不经过此处不计数）
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
        gpu_graphics_ms: gpu_sections[1],
        gpu_hiz_ms: gpu_sections[2],
        gpu_total_ms: gpu_sections[3],
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
