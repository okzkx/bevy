//! Vulkan 初始化与拆除：创建链与拆除链是同一资源束的两端——创建序即拆除序的
//! 反序，顺序契约必须放在一起看。
//!
//! | 系统 | 调度位置 | 职责 |
//! |---|---|---|
//! | `init_vulkan` | `Startup` | `try_init_vulkan` 用 `?` 串链创建资源束（**全有或全无**）；失败冒泡统一错误通道，`fatal_gate` 写 `AppExit` 优雅退出 |
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

use ash::vk;
use ash_macros::system;

use crate::{
    common::error::{Result, VulkanError},
    common::syntax::ResultTierExt,
    overlay::{AtlasGpu, UiVertexRing},
    render::{
        BindlessTables, ClusterTargets, CompositePipeline, Context, CullPass, FramePool,
        GBufferTargets, GraphicsPipeline, GpuTimer, HizPyramid, ImageCache, IndirectPass,
        BloomPipelines, BloomTargets, InstanceTables, LodPass, MeshPool, OcclusionPass,
        SmaaPipelines, SmaaTargets,
        OverlayPipeline, PresentTonemapPipeline,
        ShadowBuildPass, ShadowPipeline, ShadowTargets, TransparentPasses, TransparentPipeline,
        Swapchain, Uploader,
        ALBEDO_FORMAT, DEPTH_FORMAT, LightTable, MATPARAM_FORMAT, MAX_FRAMES_IN_FLIGHT,
        NORMAL_FORMAT, TABLE_CAPACITY,
    },};

/// 初始化入口：取主窗口句柄，串链建全套 Vulkan 资源。`NonSendMarker` 把初始化
/// 钉在主线程（Win32 句柄与 Vulkan 均主线程亲和）。
///
/// 失败统一走 Tier② 冒泡：`try_init_vulkan` 把一切失败（含时序假设被打破）折叠成
/// `VulkanError`，此处挂 context 负载后 `?` 直冒泡——Err 上交统一错误通道
/// （tier_handler error 留档 + 置位看门，fatal_gate 写 `AppExit` 优雅退出；Startup
/// 失败置位后下一次 Update 即收口，同帧退出）。
#[system]
pub(super) fn init_vulkan(
    wrapper: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    _main_thread: NonSendMarker,
    mut commands: Commands,
) -> Result<(), BevyError> {
    let (ctx, swapchain, frames, pool, uploader, image_cache, tables, instance_tables, gbuffer, pipeline, composite_pipeline, blit_pipeline, light_table, transparent_targets, transparent_passes, transparent_pipeline, overlay_pipeline, cull_pass, indirect_pass, hiz, occlusion_pass, lod_pass, shadow_targets, shadow_pipeline, shadow_build_pass, bloom_targets,
        bloom_pipelines, smaa_targets, smaa_pipelines, ui_ring, gpu_timer) =
        try_init_vulkan(&wrapper)
            .or_fatal("Vulkan 初始化失败，资源束一个都不插入")?; // 资源一个都不插入（全有或全无），Update 由 run_if 守卫跳过帧循环
    info!(
        "Vulkan 全链就绪：Context + Swapchain + {MAX_FRAMES_IN_FLIGHT} 帧在飞（含深度附件）+ MeshPool/Uploader + ImageCache + BindlessTables（常驻表，容量 {TABLE_CAPACITY}）+ InstanceTables（GPU 常驻绘制数据）+ GBufferTargets（G-buffer 附件 + HDR 中间目标，8.1）+ GraphicsPipeline + CompositePipeline + PresentBlitPipeline + ClusterTargets/TransparentPasses/TransparentPipeline（透明 Clustered Forward 层，8.1.3）+ OverlayPipeline + CullPass（视锥剔除 compute）+ IndirectPass（indirect 命令组装 compute）+ HizPyramid（Hi-Z 金字塔）+ OcclusionPass（遮挡测试 compute）+ LodPass（LOD 选档 compute，6.2）+ ShadowTargets/ShadowPipeline/ShadowBuildPass（CSM 级联阴影，8.3）+ BloomTargets/BloomPipelines/PresentTonemapPipeline（HDR 后处理，8.4）+ SmaaTargets/SmaaPipelines（SMAA 三 pass，8.5）+ UiVertexRing + GpuTimer（5.5 GPU 帧计时）；清屏+剔除+遮挡+选档+组装+绘制自下一帧（Last）起"
    );
    // 插入顺序 = 创建顺序；World 清场顺序不定，退出时的反序拆除见 teardown_vulkan
    commands.insert_resource(ctx);
    commands.insert_resource(swapchain);
    commands.insert_resource(frames);
    commands.insert_resource(pool);
    commands.insert_resource(uploader);
    commands.insert_resource(image_cache);
    commands.insert_resource(tables);
    commands.insert_resource(instance_tables);
    commands.insert_resource(gbuffer);
    commands.insert_resource(pipeline);
    commands.insert_resource(composite_pipeline);
    commands.insert_resource(blit_pipeline);
    commands.insert_resource(transparent_targets);
    commands.insert_resource(transparent_passes);
    commands.insert_resource(transparent_pipeline);
    commands.insert_resource(light_table);
    commands.insert_resource(overlay_pipeline);
    commands.insert_resource(cull_pass);
    commands.insert_resource(indirect_pass);
    commands.insert_resource(hiz);
    commands.insert_resource(occlusion_pass);
    commands.insert_resource(lod_pass);
    // Bloom 家族（8.4）：管线先于资源插（拆时反序）
    commands.insert_resource(bloom_pipelines);
    commands.insert_resource(bloom_targets);
    commands.insert_resource(smaa_pipelines);
    commands.insert_resource(smaa_targets);
    // shadow 家族（8.3）：管线先于资源插（拆时反序先拆管线再拆资源，
    // layout 借用关系与创建序一致）
    commands.insert_resource(shadow_pipeline);
    commands.insert_resource(shadow_build_pass);
    commands.insert_resource(shadow_targets);
    commands.insert_resource(ui_ring);
    commands.insert_resource(gpu_timer);
    Ok(())
}

/// 创建链本体：`?` 串起全部创建步骤，任一层失败即短路返回 `VulkanError`；
/// 失败处理集中在调用方（init_vulkan 的单点 match）。
///
/// 自 Uploader 建成起，失败路径可能有在飞 transfer（fallback 白图
/// 批次在 `BindlessTables::new` 内发票据不等待），半建资源逆序 Drop 前必须
/// 排空——否则 Uploader/表/贴图本体在 transfer 仍消费它们时析构，违反
/// "wait_idle 先于一切 Drop" 纪律。排空靠 [`InitDrainGuard`]：声明于 Uploader 之后，
/// 逆序 Drop 中最先析构；成功路径 `mem::forget` 让位给 teardown 序
///（teardown_vulkan 自带首步排空）。
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
        ctx.queue_family_index,
        ctx.queue,
        1024 * 1024,
        2,
    )?;
    // 排空护栏：此后任一步失败，护栏先于全部半建资源析构（逆序 Drop）
    let drain_guard = InitDrainGuard(ctx.device.clone());
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
    // GPU 常驻绘制数据（5.1）：对象表（DEVICE_LOCAL，staging 写）+ 模型/材质环
    //（host-visible 每帧写）+ set2 三条只读 storage（自建 pool，init 一次写入）
    let instance_tables = InstanceTables::new(
        &ctx.device,
        ctx.memory_contract(),
        &pool_sharing_families(&ctx),
    )?;
    // G-buffer 附件 + HDR 中间目标（8.1）：随帧槽分配（深度视图取自已建成的
    // FramePool），采样集以 SHADER_READ_ONLY 布局发布——合成/blit 的取数面
    let depth_views: Vec<vk::ImageView> = (0..MAX_FRAMES_IN_FLIGHT)
        .map(|s| frames.depth_view(s))
        .collect();
    let gbuffer = GBufferTargets::new(&ctx, swapchain.extent, &depth_views)?;
    // 图形管线：G-buffer 写入 pass——三 color（albedo/normal/matparam）+ 深度，
    // 声明面 = 实例附件全集（VUID-08914 纪律），布局与常驻表同源
    let pipeline = GraphicsPipeline::new(
        &ctx,
        &[ALBEDO_FORMAT, NORMAL_FORMAT, MATPARAM_FORMAT],
        DEPTH_FORMAT,
        tables.set0_layout(),
        tables.set1_layout(),
        instance_tables.set2_layout(),
    )?;
    // 光源常驻表（8.2.2）：punctual 光源 storage 表（槽位+增量账本，常驻拍板
    // 形态）——先于 cluster 资源建（描述符 b1 引用其帧槽环）
    let light_table = LightTable::new(&ctx, ctx.memory_contract())?;
    let light_ring_buffers: Vec<vk::Buffer> =
        (0..MAX_FRAMES_IN_FLIGHT).map(|s| light_table.ring_buffer(s)).collect();
    // 透明 Clustered Forward 层（8.1.3）：资源（cluster 表/透明命令链）——先于
    // 合成管线建（8.2.3 合成的 set2 = cluster 集 layout）
    let transparent_targets = ClusterTargets::new(&ctx, swapchain.extent, &light_ring_buffers)?;
    // 全屏对（8.1/8.2.3）：合成（G-buffer 光照累积 → HDR 目标，layout 借 set1 +
    // G-buffer 采样 + cluster 集）与 present blit（HDR 目标 → swapchain）——
    // 拆除序先于 GBufferTargets/ClusterTargets 与 BindlessTables（layout 借用关系）
    // shadow 资源族（8.3）：级联深度图/UBO/标记/命令环 + 描述符——先于合成
    // 管线（set layout 借用）与 shadow 命令组装/绘制管线
    let shadow_targets = ShadowTargets::new(&ctx)?;
    let composite_pipeline = CompositePipeline::new(
        &ctx,
        crate::render::HDR_FORMAT,
        tables.set1_layout(),
        gbuffer.sampled_layout(),
        transparent_targets.set_layout(),
        shadow_targets.set_layout(),
    )?;
    let shadow_pipeline = ShadowPipeline::new(
        &ctx,
        instance_tables.set2_layout(),
        shadow_targets.set_layout(),
    )?;
    let shadow_build_pass =
        ShadowBuildPass::new(&ctx, instance_tables.set2_layout(), shadow_targets.set_layout())?;
    // Bloom 资源族 + 三全屏管线（8.4）：半分辨率链目标 → 布局/描述符 →
    // prefilter/down/up；present tonemap 的 layout 借用 bloom 的 set_layout_up
    let bloom_targets = BloomTargets::new(&ctx, swapchain.extent)?;
    let bloom_pipelines = BloomPipelines::new(
        &ctx,
        gbuffer.sampled_layout(),
        bloom_targets.set_layout_tex(),
        bloom_targets.set_layout_up(),
    )?;
    // SMAA 资源族 + 三全屏管线（8.5）：LDR 目标 + edges/blend + 两 LUT——
    // present tonemap 的落点从 swapchain 改为 LDR 目标（R8G8B8A8_UNORM 线性
    // 存储），neighborhood 混合后经 swapchain SRGB view 编码恰好一次
        let smaa_targets = SmaaTargets::new(&ctx, &mut uploader, swapchain.extent)?;
        let smaa_pipelines = SmaaPipelines::new(
        &ctx,
        smaa_targets.set_layout_a(),
        smaa_targets.set_layout_b(),
        vk::Format::R8G8B8A8_UNORM,
        vk::Format::R8G8B8A8_UNORM,
        swapchain.view_format,
    )?;
    let blit_pipeline = PresentTonemapPipeline::new(
        &ctx,
        vk::Format::R8G8B8A8_UNORM,
        tables.set1_layout(),
        gbuffer.sampled_layout(),
        bloom_targets.set_layout_up(),
    )?;
    let transparent_passes = TransparentPasses::new(
        &ctx,
        transparent_targets.set_layout(),
        instance_tables.set2_layout(),
    )?;
    let transparent_pipeline = TransparentPipeline::new(
        &ctx,
        crate::render::HDR_FORMAT,
        DEPTH_FORMAT,
        tables.set0_layout(),
        tables.set1_layout(),
        instance_tables.set2_layout(),
        transparent_targets.set_layout(),
    )?;
    // overlay 管线：UI 半边的第二管线——只借常驻表 set0（无 set1），预乘混合；
    // 深度格式与渲染实例对齐但读写全关（VUID-08914），与场景管线同批拆除（layout
    // 借用 set0 layout，先于表）
    let overlay_pipeline = OverlayPipeline::new(
        &ctx,
        swapchain.view_format,
        DEPTH_FORMAT,
        tables.set0_layout(),
    )?;
    // 视锥剔除 compute 管线（5.2）：只借 set2（b0/b1/b2 只读三表 + b3 标记），
    // 平面随 112B push——前置闸门④的载体（空转首跑在首个有实例的帧收账）
    let cull_pass = CullPass::new(&ctx, instance_tables.set2_layout(), shadow_targets.set_layout())?;
    // indirect 命令组装 compute 管线（5.3）：只借 set2（b0 对象表 + b3 标记 +
    // b4 在场环 + b5 命令环），count 随 4B push——闸门②首跑在首个有在场行的帧收账
    let indirect_pass = IndirectPass::new(&ctx, instance_tables.set2_layout())?;
    // Hi-Z 金字塔（6.1.2）：×2 帧槽 R32F mip 链 + 逐级归约管线 + 遮挡测试的
    // 全链采样描述符（深度 image 须已建成——本帧槽深度即 level 0 归约源；
    // resize 随 rebuild_depth 同批重建，见 draw_frame 闸门③）
    let depth_images: Vec<vk::Image> =
        (0..MAX_FRAMES_IN_FLIGHT).map(|s| frames.depth_image(s)).collect();
    let hiz = HizPyramid::new(&ctx, swapchain.extent, &depth_images)?;
    // 遮挡测试 compute 管线（6.1）：[set2, hiz] 双集 + 80B push——hiz 的
    // occlusion layout 是它的素材（layout 借用关系，拆除序先于 hiz）
    let occlusion_pass = OcclusionPass::new(&ctx, instance_tables.set2_layout(), hiz.occlusion_layout())?;
    // LOD 选档 compute 管线（6.2）：只借 set2（b0/b1 只读 + b8 档位环读写）+
    // 96B push——首跑在首个有实例槽的帧收账（与 cull/occlusion 同款空转首跑）
    let lod_pass = LodPass::new(&ctx, instance_tables.set2_layout())?;
    // UI 顶点环：每帧槽一对 HOST_VISIBLE 顶点/索引 buffer，纯 Context
    // 依赖（初始化链尾端，拆除随帧级一批——无表/管线依赖）
    let ui_ring = UiVertexRing::new(&ctx)?;
    // GPU 帧计时（5.5）：每帧槽 TIMESTAMP query pool；队列族无 timestamp 时
    // 降级停用（enabled=false，不阻断渲染链）
    let gpu_timer = GpuTimer::new(&ctx)?;
    // 成功路径：护栏被 mem::forget 跳过 Drop，排空责任交还 teardown 序
    std::mem::forget(drain_guard);
    Ok((
        ctx, swapchain, frames, pool, uploader, image_cache, tables, instance_tables, gbuffer,
        pipeline, composite_pipeline, blit_pipeline, light_table, transparent_targets, transparent_passes,
        transparent_pipeline, overlay_pipeline, cull_pass, indirect_pass, hiz, occlusion_pass,
        lod_pass, shadow_targets, shadow_pipeline, shadow_build_pass, bloom_targets,
        bloom_pipelines, smaa_targets, smaa_pipelines, ui_ring, gpu_timer,
    ))
}

/// 初始化失败排空护栏：半建链逆序 Drop 中第一个析构（声明在 Uploader
/// 之后），Drop 时 device_wait_idle 等在飞 transfer 完成，其后 Uploader/表/
/// GpuImage 的析构才满足 "wait_idle 先于一切 Drop"。设备丢失时等待失败——错误已由初始化链冒泡，
/// 这里静默放行拆除。
struct InitDrainGuard(ash::Device);

impl Drop for InitDrainGuard {
    fn drop(&mut self) {
        unsafe { self.0.device_wait_idle() }.unwrap_or_else(|e| {
            bevy::log::warn!("初始化失败路径排空失败（设备丢失？），继续拆除: {e}")
        });
    }
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
    // Hi-Z 家族先于帧级拆：OcclusionPass 的 layout 借用 hiz 的 occlusion
    // layout（先拆管线再拆 hiz），hiz 的视图引用各帧槽深度 image（视图先于
    // 深度 image 销毁——FramePool Drop 拆深度本体）。LodPass 只借 set2
    // layout，随后于 InstanceTables 之前拆（与 CullPass 同组）。
    // 全屏对（8.1）：Composite/PresentBlit 的 layout 借用 GBufferTargets 的
    // 采样 layout 与 BindlessTables 的 set1 layout——先拆管线，再拆
    // GBufferTargets（其采样集里的深度视图必须先于 FramePool 的深度本体）。
    world.remove_resource::<OcclusionPass>();
    world.remove_resource::<HizPyramid>();
    world.remove_resource::<CompositePipeline>();
    world.remove_resource::<PresentTonemapPipeline>();
    // Bloom 家族（8.4）：BloomPipelines 的 layout 借用 BloomTargets 的 set
    // layouts——先拆管线再拆资源；资源先于 GBufferTargets（复用 ColorTarget
    // 无借用）与 BindlessTables
    world.remove_resource::<BloomPipelines>();
    world.remove_resource::<BloomTargets>();
    // SMAA 家族（8.5）：管线 layout 借用 SmaaTargets 的 set layouts——先拆
    // 管线再拆资源；LUT 直传票据随退出排空覆盖
    world.remove_resource::<SmaaPipelines>();
    world.remove_resource::<SmaaTargets>();
    // 透明层（8.1.3）：TransparentPipeline 的 layout 借用 BindlessTables/
    // InstanceTables/ClusterTargets 的 set layouts——先拆管线再拆资源
    world.remove_resource::<TransparentPipeline>();
    world.remove_resource::<TransparentPasses>();
    world.remove_resource::<ClusterTargets>();
    world.remove_resource::<LightTable>();
    world.remove_resource::<GBufferTargets>();
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
    world.remove_resource::<CullPass>();
    world.remove_resource::<IndirectPass>();
    world.remove_resource::<LodPass>();
    // shadow 家族（8.3）：管线 layout 借用 ShadowTargets 的 set layout（其又借
    // InstanceTables set2）——先拆管线再拆资源，资源先于 InstanceTables
    world.remove_resource::<ShadowBuildPass>();
    world.remove_resource::<ShadowPipeline>();
    world.remove_resource::<ShadowTargets>();
    // GPU 常驻绘制数据随管线之后拆（管线 layout 借用其 set2 layout），描述符
    // 只指向自己的三份 buffer——随字段 Drop，顺序在资产级自由
    world.remove_resource::<InstanceTables>();
    world.remove_resource::<Uploader>();
    world.remove_resource::<MeshPool>();
    world.remove_resource::<BindlessTables>();
    world.remove_resource::<ImageCache>();
    // overlay 图集 graveyard 在表之后拆：旧代随 flush 排空点定期销毁，
    // 此处只剩残量（teardown 序不变：先拆借用账本，再拆本体；overlay 未装时 no-op）
    world.remove_resource::<AtlasGpu>();
    // GPU 帧计时 query pool 随后拆（只依赖 Device，顺序相对自由；Context 必须最后）
    world.remove_resource::<GpuTimer>();
    world.remove_resource::<Context>();
    // 初始化失败路径资源从未插入，此处静默即可——根因已由统一 handler 记过 error
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
    InstanceTables,
    GBufferTargets,
    GraphicsPipeline,
    CompositePipeline,
    PresentTonemapPipeline,
    LightTable,
    ClusterTargets,
    TransparentPasses,
    TransparentPipeline,
    OverlayPipeline,
    CullPass,
    IndirectPass,
    HizPyramid,
    OcclusionPass,
    LodPass,
    ShadowTargets,
    ShadowPipeline,
    ShadowBuildPass,
    BloomTargets,
    BloomPipelines,
    SmaaTargets,
    SmaaPipelines,
    UiVertexRing,
    GpuTimer,
);
