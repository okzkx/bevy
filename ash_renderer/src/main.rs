//! 宿主壳入口（step2 施工项②③④）：禁渲染 Bevy 宿主 + ash 清屏帧循环。
//!
//! 禁用名单 = step1《DefaultPlugins分类.md》§1 的渲染族 8 件
//! （0.20.0-dev 下路径逐一复核未变，见 .agent/docs/step2-宿主壳/宿主壳搭建记录.md §2）。
//!
//! 帧循环住在 bevy runner 里（侦察篇 §4）：winit `about_to_wait` 驱动 `app.update()`，
//! 本 crate 的 Update 系统做 acquire → 清屏 → present。三个 Vulkan 资源按生命周期分层
//! —— Context（进程级）/ Swapchain（resize 级）/ FramePool（帧级）。

use ash_renderer::{
    error::VulkanError,
    frames::{FramePool, MAX_FRAMES_IN_FLIGHT},
    swapchain::{AcquireOutcome, PresentOutcome, Swapchain},
    vulkan::Context,
};
use bevy::{
    anti_alias::AntiAliasPlugin,
    app::OnAppExitSystems,
    core_pipeline::CorePipelinePlugin,
    ecs::system::NonSendMarker,
    gizmos_render::GizmoRenderPlugin,
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    pbr::PbrPlugin,
    post_process::PostProcessPlugin,
    prelude::*,
    render::{pipelined_rendering::PipelinedRenderingPlugin, RenderPlugin},
    sprite_render::SpriteRenderPlugin,
    ui_render::UiRenderPlugin,
    window::{PrimaryWindow, RawHandleWrapper, WindowResized},
};

fn main() {
    App::new()
        // 0.20 文档化的用户责任（bevy_image/src/image.rs:2467）：该资源原本由 RenderPlugin
        // 的 wgpu finish() 从 device.features() 生成，禁渲染后须自报。NONE = 诚实初值——
        // step3 接 ash 后按实际查询到的 VkFormat 支持改写。
        .insert_resource(CompressedImageFormatSupport(CompressedImageFormats::NONE))
        .add_plugins(
            DefaultPlugins.build()
                .disable::<RenderPlugin>()
                .disable::<PipelinedRenderingPlugin>()
                .disable::<CorePipelinePlugin>()
                .disable::<PostProcessPlugin>()
                .disable::<AntiAliasPlugin>()
                .disable::<SpriteRenderPlugin>()
                .disable::<UiRenderPlugin>()
                .disable::<GizmoRenderPlugin>()
                // 连带禁项（0.20.0-dev 实测）：PbrPlugin::build 无条件 load_shader_library!
                // 分配 Handle<Shader>，而 Assets<Shader> 的注册在 RenderPlugin::build（bevy_render/src/lib.rs:385）
                // ——禁 RenderPlugin 后无人注册，PbrPlugin 必炸。材质容器（Assets<StandardMaterial>）
                // 与 glTF 材质接入缝留给 step3 手动接线。
                .disable::<PbrPlugin>(),
        )
        .add_systems(Startup, (announce, init_vulkan))
        .add_systems(Update, draw_frame)
        // 退出拆除（官方先例 = bevy_time 的 silence_delayed_command_queues_on_exit）：
        // OnAppExitSystems 在 AppExit 写入之后、despawn_windows 销毁 winit 窗口之前跑——
        // Vulkan 对象必须死在 hwnd 之前（侦察篇 §3/§5 的硬边界）。
        .add_systems(
            Last,
            teardown_vulkan
                .in_set(OnAppExitSystems)
                .run_if(|messages: Res<Messages<AppExit>>| !messages.is_empty()),
        )
        .run();
}

/// 施工③：窗口句柄链入口。首窗在 runner `resumed` 回调里建好（侦察篇 §1），
/// Startup 时 `RawHandleWrapper` 必在；`NonSendMarker` 把初始化钉在主线程
///（侦察篇 §2/§3：Win32 句柄与 Vulkan 均主线程亲和）。
fn init_vulkan(
    wrapper: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    _main_thread: NonSendMarker,
    mut commands: Commands,
) {
    let Ok(wrapper) = wrapper.single() else {
        panic!("PrimaryWindow 上没有 RawHandleWrapper：窗口未在 Startup 前建好，时序假设被打破");
    };
    let ctx = match Context::new(wrapper) {
        Ok(ctx) => ctx,
        Err(e) => panic!("Vulkan 初始化失败: {e}"),
    };
    let swapchain = match Swapchain::new(&ctx) {
        Ok(swapchain) => swapchain,
        Err(e) => panic!("swapchain 创建失败: {e}"),
    };
    let frames = match FramePool::new(&ctx) {
        Ok(frames) => frames,
        Err(e) => panic!("帧资源创建失败: {e}"),
    };
    info!(
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞；清屏循环自下一 Update 起"
    );
    // 插入顺序 = 创建顺序；World 清场顺序不定，退出时的反序拆除见 teardown_vulkan
    commands.insert_resource(ctx);
    commands.insert_resource(swapchain);
    commands.insert_resource(frames);
}

/// 施工④：ash 帧循环。一次 update = 一帧：
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
    if !resized.is_empty() {
        if let Err(e) = swapchain.rebuild(&ctx) {
            bevy::log::warn!("resize 后重建失败，本帧跳过: {e}");
            return;
        }
    }

    // 1) 等本槽位上一轮提交完成，重置 fence 与命令缓冲
    if let Err(e) = frames.wait_and_reset() {
        bevy::log::error!("等待帧 fence 失败: {e}");
        return;
    }

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

    // 3) 录制 + 提交（清屏颜色随时间缓慢呼吸，肉眼可证"帧在动"）
    let image = swapchain.images[index as usize];
    let view = swapchain.views[index as usize];
    if let Err(e) = frames.record_clear_and_submit(
        &ctx,
        image,
        view,
        swapchain.extent,
        clear_color(time.elapsed_secs_f64()),
    ) {
        bevy::log::error!("录制/提交失败: {e}");
        return;
    }

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
    if rebuild_after_present || present_stale {
        if let Err(e) = swapchain.rebuild(&ctx) {
            bevy::log::warn!("present 后重建失败: {e}");
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
    world.remove_resource::<FramePool>();
    world.remove_resource::<Swapchain>();
    world.remove_resource::<Context>();
    info!("退出拆除完成：Vulkan 资源已按帧级→resize级→进程级反序移除");
}

fn announce() {
    info!("宿主壳启动：渲染族 8 插件已禁用，无 RenderApp / 无 wgpu 初始化");
}
