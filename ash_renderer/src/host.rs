//! 宿主桥（步骤 3 开工前的结构整理，自 main.rs 迁入）：bevy 调度 ↔ Vulkan 资源的接插件。
//!
//! main 只做统筹（插件组装，见 `src/main.rs`），Vulkan 侧的全部编排住本模块：
//!
//! | 时机 | 系统 | 职责 |
//! |---|---|---|
//! | `Startup` | `init_vulkan` | `try_init_vulkan` 用 `?` 串链创建三资源（**全有或全无**）；失败 → `error!` + `AppExit::error()` 优雅退出 |
//! | `Last` | `flush_uploads`（upload 模块的 `AshUploadPlugin` 注册，before 本表下一行） | 3.2.4 上传链：快照去重 → 合批 transfer 提交，先于帧循环 |
//! | `Last` | `draw_frame.run_if(resource_exists::<Context>)` | 帧循环（排在采集之后、`OnAppExitSystems` 之前）：resize 闸门（最小化整帧让路；尺寸变化帧首按需重建 swapchain+深度附件）→ 等 fence → acquire → 组装 DrawList → 录制（清屏+绘制）并提交 → present |
//! | `Last` | `teardown_vulkan.in_set(OnAppExitSystems)` | AppExit 写入后、despawn_windows 杀 hwnd 前反序拆除 |
//!
//! 本模块不持有 Vulkan 状态，只编排三个生命周期模块暴露的类型：
//! 创建链细节在 [`crate::vulkan`]：`context`（进程级）/ `swapchain`（rebuild 幂等）/
//! `frames`（录制与提交），本模块只做接线和错误分流。
//!
//! 失败策略（用户错误处理思想，两 Tier，**非必要不 panic**）：
//! ① 不影响运行 → warning 后丢弃继续（syntax 糖家族兜底；OUT_OF_DATE/SUBOPTIMAL
//!    这类"重建后重试"的次优，以及帧首重建失败都走这层）；
//! ② 影响运行 → error 冒泡到 main 优雅退出（try_init `?` 串链 → `AppExit::error()` →
//!    teardown 反序拆除、窗口自关、退出码 1；panic 的 App 析构对 Resource 是任意序，弃用）。
//!    帧循环里 acquire 成功之后的 Vulkan 真失败也归这层（D2 定案：那时 image_available
//!    已被置位、fence 时序已越过重置点，同步状态无法原样恢复，warn-继续等于"等无人
//!    signal 的 fence"死锁面；见 draw_frame 各分支）。

use bevy::{
    app::OnAppExitSystems,
    camera::{Camera, Projection},
    ecs::system::NonSendMarker,
    ecs::system::SystemParam,
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    light::{AmbientLight, DirectionalLight, GlobalAmbientLight},
    math::Mat4,
    prelude::*,
    window::{PrimaryWindow, RawHandleWrapper, WindowResized},
};

use crate::{
    error::VulkanError,
    vulkan::{
        pack_frame_uniforms, AcquireOutcome, BindlessTables, Context, DrawCall, FrameDraw,
        FramePool, FrameUniformsData, GraphicsPipeline, ImageCache, MeshPool, PushData, Swapchain,
        Uploader, FRAME_MODE_LAMBERT, FRAME_MODE_NORMAL, FRAME_MODE_UNLIT, MAX_FRAMES_IN_FLIGHT,
        TABLE_CAPACITY,
    },
};

/// 宿主桥插件：禁渲染补位（`CompressedImageFormatSupport` 自报）+ Vulkan 三系统进调度。
///
/// 禁渲染名单本身是统筹决策，留在 main；凡"禁了渲染之后必须有人补位"的运行期
/// 职责都归本插件。
pub struct AshHostPlugin;

impl Plugin for AshHostPlugin {
    fn build(&self, app: &mut App) {
        // 0.20 文档化的用户责任（bevy_image/src/image.rs:2467）：该资源原本由 RenderPlugin
        // 的 wgpu finish() 从 device.features() 生成，禁渲染后须自报。NONE = 诚实初值——
        // step3 接 ash 后按实际查询到的 VkFormat 支持改写。
        // 补位的另一半：TexturePlugin::finish（bevy_render/src/texture/mod.rs:46-60）用该资源
        // 注册真正的 ImageLoader（pending → ready）。渲染族禁用后无人注册，ImageLoader 永远
        // Pending，一切贴图 load 卡在 Loading（嵌套 recv() 无广播方，2026-09-22 实测）。
        // 贴图解码不依赖 GPU；NONE 只表示不解压缩纹理格式（basis/ktx2），png/jpeg 照常。
        app.insert_resource(CompressedImageFormatSupport(CompressedImageFormats::NONE))
            .register_asset_loader(bevy::image::ImageLoader::new(CompressedImageFormats::NONE))
            // update 节流（frenderer `try_draw_new_frame` 同款思路，用官方旋钮）：MAILBOX
            // 的 present 不再提供 vsync 背压，默认 Continuous 会让 update 以事件循环速度
            // 裸奔（实测 >200% CPU）；Reactive(1/60) 让空闲时 update 按主屏刷新率封顶，
            // 事件（拖拽/输入）仍即时驱动。最小化失焦走 reactive_low_power 同款 60Hz。
            .insert_resource(bevy::winit::WinitSettings {
                focused_mode: bevy::winit::UpdateMode::reactive(
                    std::time::Duration::from_secs_f64(1.0 / 60.0),
                ),
                unfocused_mode: bevy::winit::UpdateMode::reactive_low_power(
                    std::time::Duration::from_secs_f64(1.0 / 60.0),
                ),
            })
            .add_systems(Startup, (announce, init_vulkan))
            // 守卫：初始化失败时资源不插入（全有或全无），缺 Context 直接跳过，
            // 等 AppExit 走退出链——否则 Res<Context> 会在帧循环里 panic，优雅退出前功尽弃
            .add_systems(
                Last,
                draw_frame
                    .run_if(resource_exists::<Context>)
                    .before(OnAppExitSystems),
            );
        // 退出拆除（官方先例 = bevy_time 的 silence_delayed_command_queues_on_exit）：
        // OnAppExitSystems 在 AppExit 写入之后、despawn_windows 销毁 winit 窗口之前跑——
        // Vulkan 对象必须死在 hwnd 之前（侦察篇 §3/§5 的硬边界）。
        app.add_systems(
            Last,
            teardown_vulkan
                .in_set(OnAppExitSystems)
                .run_if(|messages: Res<Messages<AppExit>>| !messages.is_empty()),
        );
    }
}

fn announce() {
    info!("宿主壳启动：渲染族 8 插件已禁用，无 RenderApp / 无 wgpu 初始化");
}

/// 窗口句柄链入口。首窗在 runner `resumed` 回调里建好（侦察篇 §1），
/// Startup 时 `RawHandleWrapper` 必在；`NonSendMarker` 把初始化钉在主线程
///（侦察篇 §2/§3：Win32 句柄与 Vulkan 均主线程亲和）。
///
/// 失败统一走 Tier② 冒泡：`try_init_vulkan` 内部一切失败（含"时序假设被打破"——
/// 同样当可预期失败处理，工程原则**非必要不 panic**）折叠成 `VulkanError`，
/// 此处单点 match：记日志 + `AppExit::error()` 优雅退出。
fn init_vulkan(
    wrapper: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    _main_thread: NonSendMarker,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
) {
    let (ctx, swapchain, frames, pool, uploader, image_cache, tables, pipeline) =
        match try_init_vulkan(&wrapper)
        {
            Ok(ok) => ok,
            Err(e) => {
                error!("Vulkan 初始化失败，宿主壳优雅退出: {e}");
                exit.write(AppExit::error());
                return; // 资源一个都不插入（全有或全无），Update 由 run_if 守卫跳过
            }
        };
    info!(
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞（含深度附件）+ MeshPool/Uploader（3.2 上传链）+ ImageCache（3.3.1 贴图）+ BindlessTables（3.3.3 常驻表，容量 {TABLE_CAPACITY}）+ GraphicsPipeline（3.4.2）；清屏+绘制自下一帧（Last）起"
    );
    // 插入顺序 = 创建顺序；World 清场顺序不定，退出时的反序拆除见 teardown_vulkan
    commands.insert_resource(ctx);
    commands.insert_resource(swapchain);
    commands.insert_resource(frames);
    commands.insert_resource(pool);
    commands.insert_resource(uploader);
    commands.insert_resource(image_cache);
    commands.insert_resource(tables);
    commands.insert_resource(pipeline);
}

/// 初始化链路本体：`?` 串起创建链，任一层失败即短路返回 `VulkanError`——
/// 这里能优雅地用 `?`，靠的是"失败处理集中在调用方（init_vulkan 的 match）"，
/// 而不是每个系统都能 `?`（bevy 系统返回 `()`）。
/// 时序假设（`wrapper.single()`）同样当可预期失败处理：ok_or 转成 Init 错误冒泡。
/// 初始化链一次成型的资源束（顺序即创建序 = 退出反序拆除序）。
type InitChain = (
    Context,
    Swapchain,
    FramePool,
    MeshPool,
    Uploader,
    ImageCache,
    BindlessTables,
    GraphicsPipeline,
);

fn try_init_vulkan(wrapper: &Query<&RawHandleWrapper, With<PrimaryWindow>>) -> Result<InitChain, VulkanError> {
    let wrapper = wrapper.single().map_err(|_| {
        VulkanError::Init(
            "PrimaryWindow 上没有 RawHandleWrapper：窗口未在 Startup 前建好，时序假设被打破".into(),
        )
    })?;
    let ctx = Context::new(wrapper)?;
    let swapchain = Swapchain::new(&ctx)?;
    // 3.4.3 深度附件按帧槽建(尺寸取 swapchain 首建 extent,重建走 rebuild_depth)
    let frames = FramePool::new(&ctx, swapchain.extent)?;
    // 3.2 上传链:池懒建(首帧按需分配);有专用 transfer 族时两族 CONCURRENT 共享
    //(3.4 定案:所有权乒乓不值得,见 GpuBuffer::create_with_families 定案注释)
    let pool = MeshPool::new(&ctx.device, ctx.memory_contract(), &pool_sharing_families(&ctx));
    let mut uploader = Uploader::new(
        &ctx.device,
        ctx.memory_contract(),
        ctx.transfer_queue_family_index,
        ctx.transfer_queue,
        1024 * 1024,
        2,
    )?;
    // 3.3 贴图:驻留缓存空建,首帧随上传链按需进图(3.3.1)
    let image_cache = ImageCache::default();
    // 3.3.3/3.3.4:常驻描述符表(限额对账在表内做,失败即 Tier②)+ fallback 白图
    // 走同一上传链占 0 号双槽;真实贴图自首帧 flush_uploads 起从 1 号槽发布
    let tables = BindlessTables::new(
        &ctx.device,
        &ctx.instance,
        ctx.physical_device,
        ctx.memory_contract(),
        &mut uploader,
        ctx.queue_family_index,
        TABLE_CAPACITY,
    )?;
    // 3.4 跨族接线已随 CONCURRENT 定案收窄:fallback 无需登记待办 acquire
    // 3.4.2:图形管线(吃 swapchain 的渲染 view 格式与帧槽深度格式,布局与常驻表同源)
    let pipeline = GraphicsPipeline::new(
        &ctx,
        swapchain.view_format,
        crate::vulkan::DEPTH_FORMAT,
        tables.set0_layout(),
        tables.set1_layout(),
    )?;
    Ok((
        ctx, swapchain, frames, pool, uploader, image_cache, tables, pipeline,
    ))
}

/// 池 buffer 的共享族集合:有专用 transfer 族(≠graphics)时返回两族(池按
/// CONCURRENT 创建,transfer 写 + graphics 读),同族设备返回空(EXCLUSIVE)。
fn pool_sharing_families(ctx: &Context) -> Vec<u32> {
    if ctx.transfer_queue_family_index != ctx.queue_family_index {
        vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
    } else {
        Vec::new()
    }
}

/// 帧循环的 resize 闸门状态（`draw_frame` 私有，`Local` 跨帧保持）。
///
/// Windows 宿主的两个实测事实（2026-09-22，探针数据见《窗口闸门》专项篇）：
/// ① 最小化即发 `Resized(0,0)`，此后对 stale swapchain 的 `acquire` 无限阻塞
///    （驱动对不可见 surface 的 FIFO 行为）——阻塞点在 runner 的 `app.update()`
///    里，消息泵冻死，任务栏的还原点击永远轮不到处理（"最小化后无法还原"）。
/// ② FIFO present mode 下 acquire 会在显示队列满/表面失配时阻塞，拖拽中尤甚
///    （实测挂死）；MAILBOX（frenderer 同款选型）让 present 直接替换未上屏帧、
///    acquire 永不排队。
///
/// 对策：最小化整帧让路（闸门②）；尺寸变化只标 pending、帧首按需重建（rebuild
/// 幂等）——acquire 永远落在新 swapchain 上，拖拽中帧循环持续流动（用户拍板：
/// 宁要渲染卡顿，不要冻结）。
#[derive(Default)]
pub(crate) struct ResizeGate {
    /// 最新一条 resize 消息是 (0,0) = 窗口最小化中；恢复尺寸的非零消息重开闸门
    minimized: bool,
    /// 尺寸变了 / 驱动报次优，下个帧首按需重建（多设无害，rebuild 幂等）
    pending: bool,
    /// 状态转移日志去重：minimized 闸门的开关各报一次，不逐帧刷屏
    logged_minimized: bool,
}

/// draw_frame 的只读参数束（SystemParam，与 3.1.4 CollectData / 3.2 upload 的
/// UploadData 同款：参数束装下成排的只读依赖，系统签名保持精简）。
#[derive(SystemParam)]
pub(crate) struct FrameInput<'w, 's> {
    ctx: Res<'w, Context>,
    scene: Res<'w, crate::scene::CollectedScene>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    pool: Res<'w, MeshPool>,
    image_cache: Res<'w, ImageCache>,
    pipeline: Res<'w, GraphicsPipeline>,
    uploader: Res<'w, Uploader>,
    /// 相机 + 可选的每相机 `AmbientLight` 覆盖（3.5.1：挂了则压过全局资源）。
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
    /// 方向光（3.5.1：取第一盏，方向 = 实体 `back()`，bevy GPU 同款）。
    lights: Query<'w, 's, (&'static GlobalTransform, &'static DirectionalLight)>,
    ambient: Option<Res<'w, GlobalAmbientLight>>,
}

/// ash 帧循环的材质模式（3.5 定案：三态进 UBO `mode`，env `ASH_RENDER_MODE` 选）。
/// 与 `debug_draw.wgsl` 的 `MODE_*` 常量同值（`FRAME_MODE_*`）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderMode {
    /// 数据驱动 Lambert（默认）：UBO 方向光 + 环境，bevy 物理链同构。
    Lambert,
    /// albedo 直出：base color/UV/颜色空间对照（官方侧同款置 unlit）。
    Unlit,
    /// 世界法线可视化：法线方向验收仪器（非均匀缩放案例的判定仪器）。
    Normal,
}

impl RenderMode {
    fn from_env() -> Self {
        match std::env::var("ASH_RENDER_MODE").ok().as_deref() {
            None | Some("") | Some("lambert") => Self::Lambert,
            Some("unlit") => Self::Unlit,
            Some("normal") => Self::Normal,
            Some(other) => {
                // Tier①：拼写错误是配置问题不是运行故障——warn 后按默认继续，
                // 不为对照仪器的错拼中断帧循环
                bevy::log::warn!(
                    "ASH_RENDER_MODE={other:?} 无法识别（可选 lambert/unlit/normal），按 lambert 继续"
                );
                Self::Lambert
            }
        }
    }

    fn as_u32(self) -> u32 {
        match self {
            Self::Lambert => FRAME_MODE_LAMBERT,
            Self::Unlit => FRAME_MODE_UNLIT,
            Self::Normal => FRAME_MODE_NORMAL,
        }
    }
}

/// DrawList 消费状态（跨帧 `Local`）：缺资源/相机告警去重 + 材质覆盖一次性收账。
#[derive(Default)]
pub(crate) struct DrawListState {
    /// "快照未全部可画（缺驻留/缺材质）"已 warn 过（下帧自愈，不逐帧刷屏）。
    warned_pending: bool,
    /// 相机缺席告警只报一次。
    warned_no_camera: bool,
    /// 环境光缺席告警只报一次。
    warned_no_ambient: bool,
    /// 不透明调试覆盖策略报一次（首个完整 DrawList 时）。
    override_logged: bool,
    /// 3.5 灯光数据收账报一次（首个取到灯光的帧；值随帧可变，只报"链路已通"）。
    lights_logged: bool,
    /// 多方向光/无方向光的状态告警去重。
    warned_light_count: bool,
    /// 材质模式（env 每进程解析一次，`ASH_RENDER_MODE`）。
    mode: Option<RenderMode>,
}

/// ash 帧循环。一次 update = 一帧：
/// resize 闸门 → 等帧槽位空出 → acquire → 组装 DrawList → 录制（清屏+绘制）并提交 → present。
/// 资源内聚在各模块：本系统只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
///
/// 住址：`Last`（2026-09-22 自 Update 挪正）。帧内 relay 同帧闭环——Update 变更 →
/// PostUpdate 传播+采集（`scene::collect` 产快照）→ Last 提交，与官方 bevy 未开
/// 流水线的"帧末收集、同帧提交"同形；显式 `.before(OnAppExitSystems)` 钉退出帧次序。
/// 3.4.5 起 Last 消费快照：查驻留账本（MeshPool/ImageCache）+ 材质容器组装
/// DrawList（拓扑快照不跨 CPU 帧，GPU 执行异步——上传完成由票据信号量在 GPU 侧
/// 等待，CPU 不阻塞）；材质按 M2 不透明调试策略覆盖（详见 record 后的收账日志）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单，逐项声明是框架惯例，非函数签名设计味道"
)]
pub(crate) fn draw_frame(
    input: FrameInput,
    mut swapchain: ResMut<Swapchain>,
    mut frames: ResMut<FramePool>,
    mut tables: ResMut<BindlessTables>,
    mut resized: MessageReader<WindowResized>,
    time: Res<Time>,
    mut gate: Local<ResizeGate>,
    mut draw_state: Local<DrawListState>,
    mut exit: MessageWriter<AppExit>,
    _main_thread: NonSendMarker,
) {
    let FrameInput {
        ref ctx,
        ref scene,
        ref std_materials,
        ref pool,
        ref image_cache,
        ref pipeline,
        ref uploader,
        ref cameras,
        ref lights,
        ref ambient,
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
        return;
    }
    if gate.logged_minimized {
        gate.logged_minimized = false;
        info!("窗口脱离最小化：闸门重开");
    }
    // —— 闸门③：尺寸变化 → 帧首按需重建（frenderer 的 dirty_swapchain 同款时机，
    // 移到 acquire 之前）。rebuild 幂等（尺寸没变就空手而归），拖拽中每步一建、
    // 帧循环不断流——acquire 永远落在新 swapchain 上。深度附件随其后按新 extent
    // 重建（同样幂等；重建自带 device_wait_idle，在飞旧深度不可能被引用）。
    if gate.pending {
        gate.pending = false;
        if let Err(e) = swapchain.rebuild(ctx) {
            bevy::log::warn!("resize 后重建失败，下帧重试: {e}");
            gate.pending = true;
            return;
        }
        if let Err(e) = frames.rebuild_depth(ctx, swapchain.extent) {
            error!("深度附件重建失败，渲染链无法继续，优雅退出: {e}");
            exit.write(AppExit::error());
            return;
        }
    }

    // 1) 等本槽位上一轮提交完成 + 重置命令缓冲（fence 的重置在下方提交前一刻，D2）。
    // 失败 = 等待/重置层面坏掉（设备丢失、内存枯竭），继续帧循环只会撞上未知的
    // fence 状态——Tier② 冒泡退出
    if let Err(e) = frames.wait_for_slot() {
        error!("帧槽等待/重置失败，渲染链无法继续，优雅退出: {e}");
        exit.write(AppExit::error());
        return;
    }

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
            if let Err(e) = swapchain.rebuild(ctx) {
                bevy::log::warn!("acquire 过时后重建失败: {e}");
            }
            return;
        }
        Err(e) => {
            error!("acquire 失败，渲染链无法继续，优雅退出: {e}");
            exit.write(AppExit::error());
            return;
        }
    };

    // 3) 组装 DrawList（3.4.5）：同帧快照 → 驻留账本/资产容器 → push + 池引脚。
    // mesh/材质未驻留/未到货 = 该 primitive 暂缓（Tier①，下帧快照再来）；缺
    // base color 贴图 = fallback 白图照画（面板：缺资源用有效 fallback 或暂缓
    // draw，贴图缺不拦几何）。
    let mut draws: Vec<DrawCall> = Vec::with_capacity(scene.primitives.len());
    let mut pending_rows = 0usize;
    let mut alpha_override = 0usize;
    for row in &scene.primitives {
        let Some(slot) = pool.resident(row.mesh.id()) else {
            pending_rows += 1;
            continue;
        };
        let Some(material) = std_materials.get(&row.material) else {
            pending_rows += 1;
            continue;
        };
        let color = material.base_color.to_linear();
        if color.alpha < 1.0 {
            alpha_override += 1;
        }
        let slots = material
            .base_color_texture
            .as_ref()
            .and_then(|handle| image_cache.slots(handle.id()))
            .unwrap_or_else(BindlessTables::fallback_slots);
        draws.push(DrawCall {
            push: PushData {
                model: row.model.to_cols_array(),
                tex_index: slots.texture,
                sampler_index: slots.sampler,
                base_color: [color.red, color.green, color.blue, color.alpha],
            },
            vertex_base: slot.vertex_base(),
            first_index: slot.first_index(),
            index_count: slot.index_count,
        });
    }
    if pending_rows > 0 {
        if !draw_state.warned_pending {
            draw_state.warned_pending = true;
            info!(
                "DrawList 暂缓 {pending_rows}/{} 行（mesh/材质未驻留或未到货，上传链自愈后消失）",
                scene.primitives.len(),
            );
        }
    } else {
        draw_state.warned_pending = false;
    }
    if !draw_state.override_logged
        && !scene.primitives.is_empty()
        && pending_rows == 0
    {
        draw_state.override_logged = true;
        info!(
            "材质调试覆盖（M2）：{} 个 primitive 全部按不透明绘制——blend 关闭（alpha<1 的 {} 行照常画出，glTF BLEND 镜片在此列）；贴图只采 base color 槽，其余四槽不进调试着色；着色模式按 ASH_RENDER_MODE（3.5 三态，见灯光收账日志）",
            scene.primitives.len(),
            alpha_override,
        );
    }

    // 4) 相机/灯光 → 本帧槽 UBO（3.5.1）。复用安全 = wait_for_slot 的 fence：写
    // 发生在上一轮使用完成之后（帧槽轮转只是定位，安全证据是 fence）。
    // 相机缺席是场景装配问题，报一次后 identity 兜底（画面不可读但不崩帧循环）。
    // 矩阵乘序/列主序与 WGSL 端同一约定。材质模式每进程解析一次 env。
    let render_mode = *draw_state.mode.get_or_insert_with(RenderMode::from_env);
    let mut view_proj = Mat4::IDENTITY;
    let mut per_camera_ambient: Option<&AmbientLight> = None;
    match cameras.single() {
        Ok((projection, global, ambient_override)) => {
            view_proj = projection.get_clip_from_view() * global.affine().inverse();
            per_camera_ambient = ambient_override;
        }
        Err(_) => {
            if !draw_state.warned_no_camera {
                draw_state.warned_no_camera = true;
                warn!("相机缺席：view_proj 用 identity 兜底（画面不可读属预期，查 3.1.3 相机组）");
            }
        }
    }
    // —— 方向光：M2 取第一盏（多灯告警一次；排序/多灯聚光是正式光照的事）。
    // 数值约定与 bevy prepare_lights 同款：dir_to_light = 实体 back()（forward 取
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
        None => ([0.5, 1.0, 0.3, 0.0], [0.0; 4]), // 无灯：直射项归零，方向留合法占位
    };
    if light_count != 1 && !draw_state.warned_light_count && !scene.primitives.is_empty() {
        draw_state.warned_light_count = true;
        warn!("方向光 {light_count} 盏（M2 取第一盏/无灯直射项归零），多灯支持不在 M2");
    }
    // —— 环境光：相机组件 AmbientLight（若挂）压过全局 GlobalAmbientLight 资源；
    // 数值 = linear × brightness（bevy prepare_lights 同款）。
    let ambient_color = match per_camera_ambient {
        Some(a) => LinearRgba::from(a.color).to_f32_array().map(|c| c * a.brightness),
        None => match ambient.as_deref() {
            Some(global) => {
                LinearRgba::from(global.color).to_f32_array().map(|c| c * global.brightness)
            }
            None => {
                if !draw_state.warned_no_ambient {
                    draw_state.warned_no_ambient = true;
                    warn!("环境光缺席：GlobalAmbientLight 资源与相机 AmbientLight 均无，环境项归零");
                }
                [0.0; 4]
            }
        },
    };
    if !draw_state.lights_logged && light_count > 0 {
        draw_state.lights_logged = true;
        info!(
            "3.5 灯光 UBO 收账：方向光 {light_count} 盏（dir_to_light = back()，light = {}）；\
             环境 = {}（来源 {}）；材质模式 = {}（ASH_RENDER_MODE）",
            fmt_vec4(light_color),
            fmt_vec4(ambient_color),
            if per_camera_ambient.is_some() { "相机 AmbientLight" } else { "GlobalAmbientLight" },
            match render_mode {
                RenderMode::Lambert => "lambert",
                RenderMode::Unlit => "unlit",
                RenderMode::Normal => "normal",
            },
        );
    }
    if let Err(e) = tables.frame_ubo(frames.current_index()).write(
        0,
        &pack_frame_uniforms(&FrameUniformsData {
            view_proj,
            dir_to_light,
            ambient_color,
            light_color,
            mode: render_mode.as_u32(),
        }),
    ) {
        error!("帧 UBO 写失败，渲染链无法继续，优雅退出: {e}");
        exit.write(AppExit::error());
        return;
    }

    // 5) 录制 + 提交。此刻 image_available 已被 present engine 置位、image 已到手
    //——从这里起任何失败都让同步状态无法原样恢复（signal 无人等 / fence 已 reset
    // 无人 signal），按 Tier② 冒泡退出（D2：不得 warn 后继续，那是"等无人 signal
    // 的 fence"死锁面）。上传票据在 GPU 侧等待（ALL_COMMANDS），CPU 不阻塞。
    let image = swapchain.images[index as usize];
    let view = swapchain.views[index as usize];
    let frame_draw = FrameDraw {
        pipeline: pipeline.pipeline(),
        layout: pipeline.layout(),
        set0: tables.resident_set(),
        set1: tables.frame_set(frames.current_index()),
        vertex_buffer: pool.vertex_buffer(),
        index_buffer: pool.index_buffer(),
        depth_view: frames.current().depth.view,
        draws: &draws,
        wait_ticket: uploader.last_issued_ticket(),
        ticket_semaphore: uploader.ticket_semaphore(),
        clear: clear_color(time.elapsed_secs_f64()),
    };
    if let Err(e) = frames.record_frame(
        ctx,
        swapchain.render_finished[index as usize],
        image,
        view,
        swapchain.extent,
        &frame_draw,
    ) {
        error!("录制/提交失败，渲染链无法继续，优雅退出: {e}");
        exit.write(AppExit::error());
        return;
    }

    // 6) present：把画好的 image 交给 present engine，等它的信号量按 acquire 的
    // image index 取（D3），不按帧槽轮转。SUBOPTIMAL/OUT_OF_DATE 都只是标记 pending
    //（同 acquire 的次优），重建交给下个帧首的闸门③；其余真失败 Tier② 冒泡
    if let Err(e) = swapchain.present(ctx.queue, swapchain.render_finished[index as usize], index) {
        match e {
            VulkanError::SwapchainOutOfDate => gate.pending = true,
            e => {
                error!("present 失败，渲染链无法继续，优雅退出: {e}");
                exit.write(AppExit::error());
            }
        }
    }

    frames.advance();
}

/// 清屏颜色：深蓝↔青蓝慢速呼吸（周期约 10s），无任何几何也看得出每帧都在画。
fn clear_color(t: f64) -> [f32; 4] {
    let s = (t * 0.6).sin().abs() as f32;
    [0.02 + 0.03 * s, 0.06 + 0.13 * s, 0.14 + 0.22 * s, 1.0]
}

/// [f32; 4] 的收账日志格式（灯光 UBO 的 rgb 分量可读性）。
fn fmt_vec4(v: [f32; 4]) -> String {
    format!("({:.1}, {:.1}, {:.1}, {:.1})", v[0], v[1], v[2], v[3])
}

/// 退出拆除：先排空队列，再按创建的相反顺序移除资源触发 Drop——FramePool（帧级）→
/// Swapchain（resize 级）→ Context（进程级，销毁 Surface/Instance/Device）。
/// 不能等 runner `exiting` 回调的 `world.clear_all()`：那里清场顺序对 Resource 是任意的，
/// 且 winit 窗口（hwnd）已先行销毁，surface 等不到合法的宿主。
fn teardown_vulkan(world: &mut World) {
    let had_vulkan = world.get_resource::<Context>().is_some();
    // D4：第一项 GPU 资源销毁前先排空。反序拆除解决对象依赖（帧资源引用 Device），
    // device_wait_idle 解决异步使用——VUID-vkDestroyFence-fence-01120 /
    // vkDestroyCommandPool-commandPool-00041 都要求等待先于销毁，两者缺一不可。
    // WSI 边界另行记录（swapchain.rs rebuild 注释 + 缺陷文档 §4）：present 完成
    // 与队列空闲不是同一事件，严格依据（present_wait/present_fence）待 V1 后评估。
    if let Some(ctx) = world.get_resource::<Context>() {
        match unsafe { ctx.device.device_wait_idle() } {
            Ok(()) => info!("退出排空：device_wait_idle 完成，队列无在飞工作"),
            Err(e) => bevy::log::warn!("退出排空失败（设备丢失？），继续按反序拆除: {e}"),
        }
    }
    world.remove_resource::<FramePool>();
    world.remove_resource::<Swapchain>();
    // 3.2/3.3 上传链与资产件随帧级之后拆除(对象依赖只到 Device,顺序相对自由;
    // Context 必须最后——Device 归它销毁)。GraphicsPipeline 在 BindlessTables 之前:
    // 它的 layout 借用了常驻表的 set layouts,先拆管线再拆表;BindlessTables 先于
    // ImageCache:其描述符集里的 view/sampler 句柄是贴图缓存资源的借用,先拆账本
    // 再拆本体
    world.remove_resource::<GraphicsPipeline>();
    world.remove_resource::<Uploader>();
    world.remove_resource::<MeshPool>();
    world.remove_resource::<BindlessTables>();
    world.remove_resource::<ImageCache>();
    world.remove_resource::<Context>();
    // 初始化失败路径资源从未插入，此处静默即可——error! 已在 init_vulkan 记过根因
    if had_vulkan {
        info!("退出拆除完成：排空 → 帧级 → resize 级 → 管线 → 资产级(池/上传/描述符表/贴图缓存) → 进程级");
    }
}
