//! 宿主桥（步骤 3 开工前的结构整理，自 main.rs 迁入）：bevy 调度 ↔ Vulkan 资源的接插件。
//!
//! main 只做统筹（插件组装，见 `src/main.rs`），Vulkan 侧的全部编排住本模块：
//!
//! | 时机 | 系统 | 职责 |
//! |---|---|---|
//! | `Startup` | `init_vulkan` | `try_init_vulkan` 用 `?` 串链创建三资源（**全有或全无**）；失败 → `error!` + `AppExit::error()` 优雅退出 |
//! | `Update` | `draw_frame.run_if(resource_exists::<Context>)` | 一 update = 一帧：等 fence → acquire → 录制清屏并提交 → present；OUT_OF_DATE = 重建重试（控制流） |
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
    vulkan::{AcquireOutcome, Context, FramePool, MAX_FRAMES_IN_FLIGHT, PresentOutcome, Swapchain},
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
            // 守卫：初始化失败时资源不插入（全有或全无），缺 Context 本帧直接跳过，
            // 等 AppExit 走退出链——否则 Res<Context> 会在 Update 里 panic，优雅退出前功尽弃
            .add_systems(Update, draw_frame.run_if(resource_exists::<Context>));
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
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞；清屏循环自下一 Update 起"
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

/// ash 帧循环。一次 update = 一帧：
/// resize 消息驱动重建 → 等帧槽位空出 → acquire → 录制清屏并提交 → present。
/// 资源内聚在各模块：本系统只做编排和错误分流（OUT_OF_DATE 是"重试"不是"失败"）。
fn draw_frame(
    ctx: Res<Context>,
    mut swapchain: ResMut<Swapchain>,
    mut frames: ResMut<FramePool>,
    resized: MessageReader<WindowResized>,
    time: Res<Time>,
    _main_thread: NonSendMarker,
) {
    // 最小化时 extent=0：rebuild 已跳过重建，此刻没有可提交的尺寸，等恢复
    if swapchain.extent.width == 0 || swapchain.extent.height == 0 {
        return;
    }

    // bevy→Vulkan 桥：winit resize 回调暂存 → PreUpdate 入队 → 这里消费。
    // 拖拽时消息高频到达，rebuild 内部按"尺寸真变了才重建"幂等
    if !resized.is_empty()
        && let Err(e) = swapchain.rebuild(&ctx)
    {
        bevy::log::warn!("resize 后重建失败，本帧跳过: {e}");
        return;
    }

    // 1) 等本槽位上一轮提交完成，重置 fence 与命令缓冲（失败：warn 后跳过本帧）
    warn_unwrap_or_return!(frames.wait_and_reset());

    // 2) acquire：拿到一张可画的 image；OUT_OF_DATE → 重建后下一帧再来
    let mut rebuild_after_present = false;
    let index = match swapchain.acquire(frames.current().image_available) {
        Ok(AcquireOutcome::Ready(index)) => index,
        Ok(AcquireOutcome::Suboptimal(index)) => {
            rebuild_after_present = true;
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

    // 4) present：把画好的 image 交给 present engine。SUBOPTIMAL/OUT_OF_DATE 都重建；
    // acquire 阶段已报次优的，present 就算正常也要重建（下一次 acquire 大概率 OUT_OF_DATE）
    let present_stale = match swapchain.present(ctx.queue, frames.current().render_finished, index)
    {
        Ok(PresentOutcome::Done) => false,
        Ok(PresentOutcome::Suboptimal) | Err(VulkanError::SwapchainOutOfDate) => true,
        Err(e) => {
            bevy::log::error!("present 失败: {e}");
            true
        }
    };
    if (rebuild_after_present || present_stale)
        && let Err(e) = swapchain.rebuild(&ctx)
    {
        bevy::log::warn!("present 后重建失败: {e}");
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
