//! Vulkan 初始化与拆除：创建链与拆除链是同一资源束的两端——创建序即拆除序的
//! 反序，顺序契约必须放在一起看。
//!
//! | 系统 | 调度位置 | 职责 |
//! |---|---|---|
//! | `init_vulkan` | `Startup` | `try_init_vulkan` 用 `?` 串链创建资源束（**全有或全无**）；失败 → `error!` + `AppExit::error()` 优雅退出 |
//! | `teardown_vulkan` | `Last` + `OnAppExitSystems` | AppExit 写入后、窗口销毁前反序拆除 |
//!
//! 资源束见 [`InitChain`]；系统注册在 [`super::host`]（AshHostPlugin），帧循环在
//! [`super::frame`]。初始化失败路径资源一个都不插入（全有或全无），由
//! `run_if(resource_exists::<Context>)` 守卫跳过帧循环，等 AppExit 走退出链。

use bevy::{
    ecs::system::NonSendMarker,
    prelude::*,
    window::{PrimaryWindow, RawHandleWrapper},
};

use ash_macros::system;

use crate::{
    common::error::{Result, VulkanError},
    overlay::{AtlasGpu, UiVertexRing},
    vulkan::{
        BindlessTables, Context, FramePool, GraphicsPipeline, ImageCache, MeshPool,
        OverlayPipeline, Swapchain, Uploader, DEPTH_FORMAT, MAX_FRAMES_IN_FLIGHT, TABLE_CAPACITY,
    },
};

/// 初始化入口：取主窗口句柄，串链建全套 Vulkan 资源。`NonSendMarker` 把初始化
/// 钉在主线程（Win32 句柄与 Vulkan 均主线程亲和）。
///
/// 失败统一走 Tier② 冒泡：`try_init_vulkan` 把一切失败（含时序假设被打破）折叠成
/// `VulkanError`，此处单点 match：记日志 + `AppExit::error()` 优雅退出。
#[system]
pub(super) fn init_vulkan(
    wrapper: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    _main_thread: NonSendMarker,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
) {
    let (ctx, swapchain, frames, pool, uploader, image_cache, tables, pipeline, overlay_pipeline, ui_ring) =
        match try_init_vulkan(&wrapper) {
            Ok(ok) => ok,
            Err(e) => {
                error!("Vulkan 初始化失败，宿主壳优雅退出: {e}");
                exit.write(AppExit::error());
                return; // 资源一个都不插入（全有或全无），Update 由 run_if 守卫跳过
            }
        };
    info!(
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞（含深度附件）+ MeshPool/Uploader（3.2 上传链）+ ImageCache（3.3.1 贴图）+ BindlessTables（3.3.3 常驻表，容量 {TABLE_CAPACITY}）+ GraphicsPipeline（3.4.2）+ OverlayPipeline（3.7.3 overlay 管线）+ UiVertexRing（3.7.2 overlay 顶点环）；清屏+绘制自下一帧（Last）起"
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
    commands.insert_resource(overlay_pipeline);
    commands.insert_resource(ui_ring);
}

/// 创建链本体：`?` 串起全部创建步骤，任一层失败即短路返回 `VulkanError`；
/// 失败处理集中在调用方（init_vulkan 的单点 match）。
fn try_init_vulkan(
    wrapper: &Query<&RawHandleWrapper, With<PrimaryWindow>>,
) -> Result<InitChain> {
    let wrapper = wrapper.single().map_err(|_| {
        VulkanError::Init(
            "PrimaryWindow 上没有 RawHandleWrapper：窗口未在 Startup 前建好，时序假设被打破".into(),
        )
    })?;
    let ctx = Context::new(wrapper)?;
    let swapchain = Swapchain::new(&ctx)?;
    // 深度附件按帧槽建（尺寸取 swapchain 首建 extent，重建走 rebuild_depth）
    let frames = FramePool::new(&ctx, swapchain.extent)?;
    // 上传链：池懒建（首帧按需分配）；有专用 transfer 族时两族 CONCURRENT 共享
    //（所有权乒乓不值得，见 GpuBuffer::create_with_families 注释）
    let pool = MeshPool::new(&ctx.device, ctx.memory_contract(), &pool_sharing_families(&ctx));
    let mut uploader = Uploader::new(
        &ctx.device,
        ctx.memory_contract(),
        ctx.transfer_queue_family_index,
        ctx.transfer_queue,
        1024 * 1024,
        2,
    )?;
    // 贴图：驻留缓存空建，首帧随上传链按需进图
    let image_cache = ImageCache::default();
    // 常驻描述符表（限额对账在表内做，失败即 Tier②）+ fallback 白图走同一上传链
    // 占 0 号双槽；真实贴图自首帧 flush_uploads 起从 1 号槽发布
    let tables = BindlessTables::new(
        &ctx.device,
        &ctx.instance,
        ctx.physical_device,
        ctx.memory_contract(),
        &mut uploader,
        ctx.queue_family_index,
        TABLE_CAPACITY,
    )?;
    // 图形管线：吃 swapchain 的渲染 view 格式与帧槽深度格式，布局与常驻表同源
    let pipeline = GraphicsPipeline::new(
        &ctx,
        swapchain.view_format,
        DEPTH_FORMAT,
        tables.set0_layout(),
        tables.set1_layout(),
    )?;
    // overlay 管线（3.7）：UI 半边的第二管线——只借常驻表 set0（无 set1），预乘混合；
    // 深度格式与渲染实例对齐但读写全关（VUID-08914），与场景管线同批拆除（layout
    // 借用 set0 layout，先于表）
    let overlay_pipeline = OverlayPipeline::new(
        &ctx,
        swapchain.view_format,
        DEPTH_FORMAT,
        tables.set0_layout(),
    )?;
    // UI 顶点环（3.7.2）：每帧槽一对 HOST_VISIBLE 顶点/索引 buffer，纯 Context
    // 依赖（初始化链尾端，拆除随帧级一批——无表/管线依赖）
    let ui_ring = UiVertexRing::new(&ctx)?;
    Ok((
        ctx, swapchain, frames, pool, uploader, image_cache, tables, pipeline, overlay_pipeline,
        ui_ring,
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

/// 退出拆除：先排空队列，再按创建的相反顺序移除资源触发 Drop——FramePool（帧级）→
/// Swapchain（resize 级）→ Context（进程级，销毁 Surface/Instance/Device）。
/// 不能等 runner `exiting` 回调的 `world.clear_all()`：那里清场顺序对 Resource 是任意的，
/// 且 winit 窗口（hwnd）已先行销毁，surface 等不到合法的宿主。
#[system]
pub(super) fn teardown_vulkan(world: &mut World) {
    let had_vulkan = world.get_resource::<Context>().is_some();
    // 第一项 GPU 资源销毁前先排空。反序拆除解决对象依赖（帧资源引用 Device），
    // device_wait_idle 解决异步使用（在飞命令引用着待销毁对象）——两者缺一不可。
    // 注意 wait_idle 不担保 present engine 已消费完 render_finished（WSI 边界，
    // 详见 swapchain.rs rebuild 注释）。
    if let Some(ctx) = world.get_resource::<Context>() {
        match unsafe { ctx.device.device_wait_idle() } {
            Ok(()) => info!("退出排空：device_wait_idle 完成，队列无在飞工作"),
            Err(e) => bevy::log::warn!("退出排空失败（设备丢失？），继续按反序拆除: {e}"),
        }
    }
    world.remove_resource::<FramePool>();
    // UI 顶点环随帧级一批拆（帧内使用，无表/管线依赖；overlay 未装时缺席即 no-op）
    world.remove_resource::<UiVertexRing>();
    world.remove_resource::<Swapchain>();
    // 上传链与资产件随帧级之后拆除（对象依赖只到 Device，顺序相对自由；
    // Context 必须最后——Device 归它销毁）。两根管线在 BindlessTables 之前:
    // 它们的 layout 借用了常驻表的 set layouts,先拆管线再拆表;BindlessTables 先于
    // ImageCache:其描述符集里的 view/sampler 句柄是贴图缓存资源的借用,先拆账本
    // 再拆本体
    world.remove_resource::<OverlayPipeline>();
    world.remove_resource::<GraphicsPipeline>();
    world.remove_resource::<Uploader>();
    world.remove_resource::<MeshPool>();
    world.remove_resource::<BindlessTables>();
    world.remove_resource::<ImageCache>();
    // overlay 图集 graveyard 在表之后拆：槽位持有的是 view/sampler 句柄借用，
    // 与 bindless 表同寿同序（先拆借用账本，再拆本体；overlay 未装时 no-op）
    world.remove_resource::<AtlasGpu>();
    world.remove_resource::<Context>();
    // 初始化失败路径资源从未插入，此处静默即可——error! 已在 init_vulkan 记过根因
    if had_vulkan {
        info!("退出拆除完成：排空 → 帧级(帧池/UI 顶点环) → resize 级 → 管线(场景/overlay) → 资产级(池/上传/描述符表/贴图缓存/图集 graveyard) → 进程级");
    }
}

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
    OverlayPipeline,
    UiVertexRing,
);
