//! 宿主桥（步骤 3 开工前的结构整理，自 main.rs 迁入）：bevy 调度 ↔ Vulkan 资源的接插件。
//!
//! main 只做统筹（插件组装，见 `src/main.rs`），Vulkan 侧的全部编排住本模块：
//!
//! | 时机 | 系统 | 职责 |
//! |---|---|---|
//! | `Startup` | `init_vulkan` | `try_init_vulkan` 用 `?` 串链创建三资源（**全有或全无**）；失败 → `error!` + `AppExit::error()` 优雅退出 |
//! | `Last` | `draw_frame.run_if(resource_exists::<Context>)` | 帧循环（排在采集之后、`OnAppExitSystems` 之前）：resize 闸门（最小化让路 / 停歇重建）→ 等 fence → acquire → 录制清屏并提交 → present |
//! | `Last` | `teardown_vulkan.in_set(OnAppExitSystems)` | AppExit 写入后、despawn_windows 杀 hwnd 前反序拆除 |
//!
//! 本模块不持有 Vulkan 状态，只编排三个生命周期模块暴露的类型：
//! 创建链细节在 [`crate::vulkan`]：`context`（进程级）/ `swapchain`（rebuild 幂等）/
//! `frames`（录制与提交），本模块只做接线和错误分流。
//!
//! 失败策略（用户错误处理思想，两 Tier，**非必要不 panic**）：
//! ① 不影响运行 → warning 后丢弃继续（syntax 糖家族兜底）；
//! ② 影响运行 → error 冒泡到 main 优雅退出（try_init `?` 串链 → `AppExit::error()` →
//!    teardown 反序拆除、窗口自关、退出码 1；panic 的 App 析构对 Resource 是任意序，弃用）。

use std::time::{Duration, Instant};

use bevy::{
    app::OnAppExitSystems,
    ecs::system::NonSendMarker,
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    prelude::*,
    window::{PrimaryWindow, RawHandleWrapper, WindowResized},
};

use crate::{
    error::VulkanError,
    syntax::warn_unwrap_or_return,
    vulkan::{AcquireOutcome, Context, FramePool, MAX_FRAMES_IN_FLIGHT, Swapchain},
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
    let (ctx, swapchain, frames) = match try_init_vulkan(&wrapper) {
        Ok(ok) => ok,
        Err(e) => {
            error!("Vulkan 初始化失败，宿主壳优雅退出: {e}");
            exit.write(AppExit::error());
            return; // 资源一个都不插入（全有或全无），Update 由 run_if 守卫跳过
        }
    };
    info!(
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞；清屏循环自下一帧（Last）起"
    );
    // 插入顺序 = 创建顺序；World 清场顺序不定，退出时的反序拆除见 teardown_vulkan
    commands.insert_resource(ctx);
    commands.insert_resource(swapchain);
    commands.insert_resource(frames);
}

/// 初始化链路本体：`?` 串起创建链，任一层失败即短路返回 `VulkanError`——
/// 这里能优雅地用 `?`，靠的是"失败处理集中在调用方（init_vulkan 的 match）"，
/// 而不是每个系统都能 `?`（bevy 系统返回 `()`）。
/// 时序假设（`wrapper.single()`）同样当可预期失败处理：ok_or 转成 Init 错误冒泡。
fn try_init_vulkan(
    wrapper: &Query<&RawHandleWrapper, With<PrimaryWindow>>,
) -> Result<(Context, Swapchain, FramePool), VulkanError> {
    let wrapper = wrapper.single().map_err(|_| {
        VulkanError::Init("PrimaryWindow 上没有 RawHandleWrapper：窗口未在 Startup 前建好，时序假设被打破".into())
    })?;
    let ctx = Context::new(wrapper)?;
    let swapchain = Swapchain::new(&ctx)?;
    let frames = FramePool::new(&ctx)?;
    Ok((ctx, swapchain, frames))
}

/// 最后一条 resize 消息之后隔多久才算"拖拽停歇"、允许重建（实测标定：重建一次
/// ≈ wait_idle 32ms + destroy 6ms + create 22ms，拖拽中每步重建就是卡顿感的来源；
/// 调参点：调小 → 内容跟上更快但中途插入更多停顿）。
const DRAG_QUIET: Duration = Duration::from_millis(150);

/// 帧循环的 resize 闸门状态（`draw_frame` 私有，`Local` 跨帧保持）。
///
/// Windows 宿主的三个实测事实（2026-09-22，探针数据见施工记录）：
/// ① 最小化即发 `Resized(0,0)`（winit event_loop.rs WM_SIZE 无条件转发），
///    且此后对 stale swapchain 的 `acquire` **无限阻塞**（驱动对不可见 surface 的
///    FIFO 行为）——阻塞点在 runner 的 `app.update()` 里，消息泵随之冻死，任务栏
///    的还原点击（WM_SYSCOMMAND）永远轮不到处理，即"最小化后无法还原"的死锁。
/// ② 尺寸失配不限于最小化：拖拽中若继续 acquire/present 与窗口尺寸失配的
///    swapchain，同样会阻塞（实测拖拽第 3 步挂死）——**尺寸不一致期间整个
///    present 通道都不可信，必须整帧让路**。
/// ③ 拖拽中每步重建 ≈60ms（wait_idle+destroy+create），每步都建就是"resize
///    卡顿感"的来源；停歇后一次重建到位则完全无感。
///
/// 对策统一进闸门：窗口尺寸与 swapchain 不一致期间（消息持续到达、未停歇），
/// 整帧让路不碰 Vulkan——画面由 DWM 持有的最后一帧拉伸顶住（主流引擎同款）；
/// 消息停歇 [`DRAG_QUIET`] 后重建一次再恢复渲染。
#[derive(Default)]
struct ResizeGate {
    /// 最新一条 resize 消息是 (0,0) = 窗口最小化中；恢复尺寸的非零消息重开闸门
    minimized: bool,
    /// 尺寸变了 / 驱动报次优，等停歇后要重建一次（rebuild 幂等，多设无害）
    pending: bool,
    /// 最后一条非零 resize 消息的时刻，用于判定拖拽停歇
    last_resize: Option<Instant>,
    /// 状态转移日志去重：minimized 闸门的开关各报一次，不逐帧刷屏
    logged_minimized: bool,
}

/// ash 帧循环。一次 update = 一帧：
/// resize 闸门 → 等帧槽位空出 → acquire → 录制清屏并提交 → present。
/// 资源内聚在各模块：本系统只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
///
/// 住址：`Last`（2026-09-22 自 Update 挪正）。帧内 relay 同帧闭环——Update 变更 →
/// PostUpdate 传播+采集（`scene::collect` 产快照）→ Last 提交，与官方 bevy 未开
/// 流水线的"帧末收集、同帧提交"同形；原 Update 钉位是步骤 2 清屏时代的产物，
/// "N 帧末采、N+1 帧初画"的跨帧滞后在单线程宿主里买不到任何并行（伪流水线纯支出），
/// 故归位。显式 `.before(OnAppExitSystems)` 钉退出帧次序：本帧照常画完，teardown
/// 才反序拆除。resize 消息不受影响——缓冲在 `First` 换（bevy_app/src/sub_app.rs），
/// winit 回调写入的消息本帧 Update/Last 都可读。
fn draw_frame(
    ctx: Res<Context>,
    mut swapchain: ResMut<Swapchain>,
    mut frames: ResMut<FramePool>,
    mut resized: MessageReader<WindowResized>,
    time: Res<Time>,
    mut gate: Local<ResizeGate>,
    _main_thread: NonSendMarker,
) {
    // —— 闸门①：读 resize 消息。一次 update 可能积压多条（拖拽 125Hz 输入 vs 60fps
    // 帧），逐条刷状态，最新一条定生死：非零尺寸 = 正常/恢复，(0,0) = 最小化。
    for msg in resized.read() {
        if msg.width == 0.0 || msg.height == 0.0 {
            gate.minimized = true;
        } else {
            gate.minimized = false;
            gate.pending = true;
            gate.last_resize = Some(Instant::now());
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
    // —— 闸门③：尺寸不一致期间整帧让路（事实②），停歇后重建一次（事实③）。
    // 拖拽进行中不重建也不绘制——对失配 swapchain 的 acquire/present 会阻塞；
    // 画面由 DWM 持有的最后一帧拉伸顶住。rebuild 幂等（尺寸没变就空手而归）。
    // acquire 报的 OUT_OF_DATE 走不到这里（下方立即重建），那个状态连一帧都画不了。
    let quiet = gate.last_resize.is_none_or(|t| t.elapsed() >= DRAG_QUIET);
    if gate.pending {
        if !quiet {
            return;
        }
        gate.pending = false;
        info!("resize 停歇（≥{DRAG_QUIET:?}）：按需重建 swapchain（尺寸未变则空过）");
        if let Err(e) = swapchain.rebuild(&ctx) {
            bevy::log::warn!("resize 后重建失败，下帧重试: {e}");
            gate.pending = true;
            return;
        }
    }

    // 1) 等本槽位上一轮提交完成，重置 fence 与命令缓冲（失败：warn 后跳过本帧）
    warn_unwrap_or_return!(frames.wait_and_reset());

    // 2) acquire：拿到一张可画的 image；OUT_OF_DATE = 这个 swapchain 已不可用
    //（最小化/恢复/独占模式切换等），必须立即重建——去抖等不了它
    let index = match swapchain.acquire(frames.current().image_available) {
        Ok(AcquireOutcome::Ready(index)) => index,
        Ok(AcquireOutcome::Suboptimal(index)) => {
            // 拿得到图但尺寸不理想：照常画完这帧（DWM 拉伸顶住），标 pending——
            // 停歇前闸门③会拦住后续帧，不会再碰这个失配的 swapchain
            gate.pending = true;
            index
        }
        Err(VulkanError::SwapchainOutOfDate) => {
            if let Err(e) = swapchain.rebuild(&ctx) {
                bevy::log::warn!("acquire 过时后重建失败: {e}");
            }
            return;
        }
        Err(e) => {
            bevy::log::error!("acquire 失败: {e}");
            return;
        }
    };

    // 3) 录制 + 提交（清屏颜色随时间缓慢呼吸，肉眼可证"帧在动"；失败：warn 后跳过本帧）
    let image = swapchain.images[index as usize];
    let view = swapchain.views[index as usize];
    warn_unwrap_or_return!(frames.record_clear_and_submit(
        &ctx,
        image,
        view,
        swapchain.extent,
        clear_color(time.elapsed_secs_f64()),
    ));

    // 4) present：把画好的 image 交给 present engine。SUBOPTIMAL/OUT_OF_DATE 都只是
    // 标记 pending（同 acquire 的次优），重建交给闸门③的停歇时机
    if let Err(e) = swapchain.present(ctx.queue, frames.current().render_finished, index) {
        match e {
            VulkanError::SwapchainOutOfDate => gate.pending = true,
            e => bevy::log::error!("present 失败: {e}"),
        }
    }

    frames.advance();
}

/// 清屏颜色：深蓝↔青蓝慢速呼吸（周期约 10s），无任何几何也看得出每帧都在画。
fn clear_color(t: f64) -> [f32; 4] {
    let s = (t * 0.6).sin().abs() as f32;
    [0.02 + 0.03 * s, 0.06 + 0.13 * s, 0.14 + 0.22 * s, 1.0]
}

/// 退出拆除：按创建的相反顺序移除资源触发 Drop——FramePool（帧级）→ Swapchain
/// （resize 级）→ Context（进程级，销毁 Surface/Instance/Device）。
/// 不能等 runner `exiting` 回调的 `world.clear_all()`：那里清场顺序对 Resource 是任意的，
/// 且 winit 窗口（hwnd）已先行销毁，surface 等不到合法的宿主。
fn teardown_vulkan(world: &mut World) {
    let had_vulkan = world.get_resource::<Context>().is_some();
    world.remove_resource::<FramePool>();
    world.remove_resource::<Swapchain>();
    world.remove_resource::<Context>();
    // 初始化失败路径资源从未插入，此处静默即可——error! 已在 init_vulkan 记过根因
    if had_vulkan {
        info!("退出拆除完成：Vulkan 资源已按帧级→resize级→进程级反序移除");
    }
}
