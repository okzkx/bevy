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
    scene::InstanceLedger,
    vulkan::{
        pack_frame_uniforms, AcquireOutcome, BindlessTables, Context, FrameDraw,
        FramePool, FrameUniformsData, GraphicsPipeline, ImageCache, InstanceTables, MeshPool,
        OverlayPipeline, Swapchain, Uploader,
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
        gate.pending = false;
    }

    // 1) 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在下方提交前一刻）。
    // 失败 = 等待/重置层面坏掉（设备丢失、内存枯竭），继续帧循环只会撞上未知的
    // fence 状态——Tier②
    frames
        .wait_for_slot()
        .or_fatal("帧槽等待/重置失败，渲染链无法继续")?;

    // 2) acquire：拿到一张可画的 image；OUT_OF_DATE = 这个 swapchain 已不可用
    //（最小化/恢复/独占模式切换等），必须立即重建——等不得下一帧
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

    // 3) 组装 DrawList：槽位枚举（账本行 → 实例槽 + 池引脚；逐对象参数已进
    // GPU 常驻表，push 只剩 4B 实例槽号——见 drawlist 模块文档）。可绘判定
    // 与 4.x 语义一致：实例槽缺/mesh 未驻留/材质缺席 = 暂缓（Tier① 自愈）。
    let assembly = drawlist::assemble(ledger, pool, std_materials);
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
    match cameras.single() {
        Ok((projection, global, ambient_override)) => {
            view_proj = projection.get_clip_from_view() * global.affine().inverse();
            per_camera_ambient = ambient_override;
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
    drawlist::write_rings(
        instance_tables,
        ledger,
        std_materials,
        image_cache,
        &assembly,
        frames.current_index(),
    )
    .or_fatal("模型/材质环写失败，渲染链无法继续")?;

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
        draws: &assembly.draws,
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
    };
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

    // 6) present：把画好的 image 交给 present engine，等它的信号量按 acquire 的
    // image index 取（不按帧槽轮转）。SUBOPTIMAL/OUT_OF_DATE 都只是标记 pending
    //（同 acquire 的次优），重建交给下个帧首的闸门③；其余真失败不进通道也不
    // 提前返回——advance 必须照常推进保持帧槽账本一致，就地 error 留档 + 置位
    // 看门，退出由下一帧 Update 的 fatal_gate 生效（判定线：不提前 return 的
    // 失败不冒泡，就地处理）。
    if let Err(e) = swapchain.present(ctx.queue, swapchain.render_finished[index as usize], index) {
        match e {
            VulkanError::SwapchainOutOfDate => gate.pending = true,
            e => {
                error!("present 失败，渲染链无法继续: {e}");
                request_graceful_exit();
            }
        }
    }

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
