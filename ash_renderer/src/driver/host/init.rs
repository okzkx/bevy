//! Vulkan 资源的创建与拆除。创建顺序和拆除顺序放在同一个文件里对照着看。
//!
//! - `init_vulkan`（Startup）：一次建好全部资源，全成功才插入 World，任一步失败就一个都不插、程序退出。
//! - `teardown_vulkan`（退出时）：在窗口销毁前，按 `init_chain!` 表里的顺序逐个拆除。

use bevy::{
    ecs::system::NonSendMarker,
    prelude::*,
    window::{PrimaryWindow, RawHandleWrapper},
};

use ash::vk;
use ash_macros::system;

use crate::{
    common::error::Result,
    common::syntax::ResultTierExt,
    render::{
        AtlasGpu, BindlessTables, ClusterBuildPass, CompositePipeline, Context, CullPass, FramePool,
        GBufferTargets, GBufferPipeline, GpuTimer, HizPyramid, ImageCache, IndirectPass,
        BloomPipelines, BloomTargets, InstanceTables, LodPass, MeshPool, OcclusionPass,
        SmaaPipelines, SmaaTargets,
        OverlayPipeline, PresentTonemapPipeline, SetLayouts,
        ShadowBuildPass, ShadowPipeline, ShadowTargets, TransparentPasses, TransparentPipeline,
        TransparentTargets, LightClusters,
        Swapchain, UiVertexRing, Uploader,
        ALBEDO_FORMAT, DEPTH_FORMAT, LightTable, MATPARAM_FORMAT, MAX_FRAMES_IN_FLIGHT,
        NORMAL_FORMAT, TABLE_CAPACITY,
    },
};

/// 取主窗口句柄，建好全部 Vulkan 资源后插入 World；任一步失败直接报错退出。
///
/// 坑：窗口句柄不能改用 `Single` 参数——取不到时整个 system 会被静默跳过，程序既不初始化也不退出。
#[system]
pub(super) fn init_vulkan(
    wrapper: Query<&RawHandleWrapper, With<PrimaryWindow>>,
    _main_thread: NonSendMarker,
    mut commands: Commands,
) -> Result<(), BevyError> {
    let wrapper = wrapper
        .single()
        .or_fatal("PrimaryWindow 上取不到唯一的 RawHandleWrapper：窗口未在 Startup 前建好，时序假设被打破")?;
    let chain = try_init_vulkan(wrapper).or_fatal("Vulkan 初始化失败，资源束一个都不插入")?;
    let names = chain.insert_all(&mut commands);
    info!(
        "Vulkan 全链就绪（{MAX_FRAMES_IN_FLIGHT} 帧在飞，常驻表容量 {TABLE_CAPACITY}）：{}；清屏+剔除+遮挡+选档+组装+绘制自下一帧（Last）起",
        names.join(" + "),
    );
    Ok(())
}

/// 创建链按五类分段：①设备根 → ②帧级执行件 → ③常驻数据表 → ④渲染目标 → ⑤管线。
/// 段内与跨段都保持依赖序：后建的会用到先建的（layout、视图等），跨段的顺序不能调。
fn try_init_vulkan(wrapper: &RawHandleWrapper) -> Result<InitChain> {
    // ---- ① 设备根：全局唯一，其余四类都向它借 Device ----
    let ctx = Context::new(wrapper)?;
    let swapchain = Swapchain::new(&ctx)?;

    // ---- ② 帧级执行件：按在飞帧数各开一份，每帧轮换 ----
    let frames = FramePool::new(&ctx, swapchain.extent)?;
    let mut uploader = Uploader::new(
        &ctx.device,
        ctx.memory_contract(),
        ctx.transfer_queue_family_index,
        ctx.transfer_queue,
        ctx.queue_family_index,
        ctx.queue,
        1024 * 1024, // 暂存区 1MB
        2,           // 上传槽数
    )?;
    let ui_ring = UiVertexRing::new(&ctx)?;
    // 设备不支持时间戳查询时自动停用，不影响渲染
    let gpu_timer = GpuTimer::new(&ctx)?;

    // ---- ③ 常驻数据表：跨帧常驻的大 buffer + 槽位账，向 ⑤ 出借 set 布局 ----
    let pool = MeshPool::new(&ctx.device, ctx.memory_contract(), &ctx.sharing_families());
    let image_cache = ImageCache::default();
    // 会提交上传（构造内当场等完）：0 号槽的白色兜底贴图
    let tables = BindlessTables::new(
        &ctx.device,
        &ctx.instance,
        ctx.physical_device,
        ctx.memory_contract(),
        &mut uploader,
        ctx.queue_family_index,
        TABLE_CAPACITY,
    )?;
    let instance_tables = InstanceTables::new(
        &ctx.device,
        ctx.memory_contract(),
        &ctx.sharing_families(),
    )?;
    let light_table = LightTable::new(&ctx, ctx.memory_contract())?;

    // ---- ④ 渲染目标：各渲染效果自己的资源族，向 ⑤ 出借采样/数据布局 ----
    let depth_views: Vec<vk::ImageView> = (0..MAX_FRAMES_IN_FLIGHT)
        .map(|s| frames.depth_view(s))
        .collect();
    let gbuffer = GBufferTargets::new(&ctx, swapchain.extent, &depth_views)?;
    let light_ring_buffers: Vec<vk::Buffer> =
        (0..MAX_FRAMES_IN_FLIGHT).map(|s| light_table.ring_buffer(s)).collect();
    let light_clusters = LightClusters::new(&ctx, swapchain.extent, &light_ring_buffers)?;
    let transparent_targets = TransparentTargets::new(&ctx)?;
    let shadow_targets = ShadowTargets::new(&ctx)?;
    let bloom_targets = BloomTargets::new(&ctx, swapchain.extent)?;
    // 会提交上传（构造内当场等完）：两张查找表贴图
    let smaa_targets = SmaaTargets::new(&ctx, &mut uploader, swapchain.extent)?;
    let depth_images: Vec<vk::Image> =
        (0..MAX_FRAMES_IN_FLIGHT).map(|s| frames.depth_image(s)).collect();
    let hiz = HizPyramid::new(&ctx, swapchain.extent, &depth_images)?;

    // ---- ⑤ 管线：无状态，只借 ③④ 的描述符布局。③④ 先把布局登记进 SetLayouts，
    // 各管线按自己的布局配方取用并与 shader ABI 对账。声明的附件格式必须和实际
    // 渲染用的附件完全一致，否则验证层报 VUID-08914 ----
    let layouts = SetLayouts::new([
        tables.bindless_layout(),
        tables.frame_layout(),
        instance_tables.set_layout(),
        gbuffer.sampled_layout(),
        light_clusters.set_layout(),
        transparent_targets.set_layout(),
        shadow_targets.set_layout(),
        bloom_targets.tex_layout(),
        bloom_targets.up_layout(),
        smaa_targets.layout_a(),
        smaa_targets.layout_b(),
        hiz.occlusion_layout(),
    ])?;
    // 图形管线（PipelineHandle）
    let gbuffer_pipeline =
        GBufferPipeline::new(&ctx, &layouts, &[ALBEDO_FORMAT, NORMAL_FORMAT, MATPARAM_FORMAT], DEPTH_FORMAT)?;
    let shadow_pipeline = ShadowPipeline::new(&ctx, &layouts)?;
    let transparent_pipeline = TransparentPipeline::new(&ctx, &layouts, crate::render::HDR_FORMAT, DEPTH_FORMAT)?;
    // overlay 不用深度，但深度格式仍要和实际附件一致（同上 VUID-08914）
    let overlay_pipeline = OverlayPipeline::new(&ctx, &layouts, swapchain.view_format, DEPTH_FORMAT)?;
    // 全屏管线（FullscreenPipeline）
    let composite_pipeline = CompositePipeline::new(&ctx, &layouts, crate::render::HDR_FORMAT)?;
    let bloom_pipelines = BloomPipelines::new(&ctx, &layouts)?;
    // tonemap 输出存线性值，最后写屏幕时由 SRGB 视图做唯一一次 sRGB 编码
    let smaa_pipelines = SmaaPipelines::new(
        &ctx,
        &layouts,
        vk::Format::R8G8B8A8_UNORM,
        vk::Format::R8G8B8A8_UNORM,
        swapchain.view_format,
    )?;
    let tonemap_pipeline = PresentTonemapPipeline::new(&ctx, &layouts, vk::Format::R8G8B8A8_UNORM)?;
    // Compute 管线（ComputePipeline）
    let cull_pass = CullPass::new(&ctx, &layouts)?;
    let indirect_pass = IndirectPass::new(&ctx, &layouts)?;
    let shadow_build_pass = ShadowBuildPass::new(&ctx, &layouts)?;
    let occlusion_pass = OcclusionPass::new(&ctx, &layouts)?;
    let lod_pass = LodPass::new(&ctx, &layouts)?;
    let cluster_build_pass = ClusterBuildPass::new(&ctx, &layouts)?;
    let transparent_passes = TransparentPasses::new(&ctx, &layouts)?;

    Ok(InitChain {
        ctx,
        swapchain,
        frames,
        uploader,
        ui_ring,
        gpu_timer,
        pool,
        image_cache,
        tables,
        instance_tables,
        light_table,
        gbuffer,
        light_clusters,
        transparent_targets,
        shadow_targets,
        bloom_targets,
        smaa_targets,
        hiz,
        gbuffer_pipeline,
        shadow_pipeline,
        transparent_pipeline,
        overlay_pipeline,
        composite_pipeline,
        bloom_pipelines,
        smaa_pipelines,
        tonemap_pipeline,
        cull_pass,
        indirect_pass,
        shadow_build_pass,
        occlusion_pass,
        lod_pass,
        cluster_build_pass,
        transparent_passes,
    })
}

/// 退出时先等 GPU 空闲，再按 `init_chain!` 表的顺序逐个移除资源。
///
/// 坑：不能等 Bevy 退出时自动清理——那时资源释放顺序不确定，而且窗口已经先销毁了。
/// 坑：等 GPU 空闲不代表显示端已用完交换链，详见 `Swapchain::rebuild`。
#[system]
pub(super) fn teardown_vulkan(world: &mut World) {
    let had_vulkan = world.get_resource::<Context>().is_some();
    if let Some(ctx) = world.get_resource::<Context>() {
        match unsafe { ctx.device.device_wait_idle() } {
            Ok(()) => info!("退出排空：device_wait_idle 完成，队列无在飞工作"),
            Err(e) => bevy::log::warn!("退出排空失败（设备丢失？），继续按反序拆除: {e}"),
        }
    }
    // 初始化失败时资源从未插入，这里的移除都是空操作
    InitChain::remove_all(world);
    if had_vulkan {
        info!("退出拆除完成：排空 → 帧级(帧池/UI 顶点环) → resize 级 → 管线(场景/overlay) → 资产级(池/上传/描述符表/贴图缓存/图集 graveyard) → 进程级");
    }
}

/// 资源表：写成 `类型 => 字段` 的由创建链产出；只写类型的由别处插入，这里只负责拆除。
/// 表中顺序就是拆除顺序。
macro_rules! init_chain {
    ($($ty:ty $(=> $field:ident)?),* $(,)?) => {
        struct InitChain {
            $($($field: $ty,)?)*
        }

        impl InitChain {
            /// 全部插入 World，返回类型名清单供日志使用。
            fn insert_all(self, commands: &mut Commands) -> Vec<&'static str> {
                vec![$($({
                    commands.insert_resource(self.$field);
                    stringify!($ty)
                },)?)*]
            }

            /// 按表中顺序移除；不存在的资源直接跳过。
            fn remove_all(world: &mut World) {
                $(world.remove_resource::<$ty>();)*
            }
        }
    };
}

// 拆除顺序规则：谁用了别人的东西（layout、视图等），谁就先拆；Context 最后拆。
init_chain! {
    OcclusionPass => occlusion_pass,
    HizPyramid => hiz,
    CompositePipeline => composite_pipeline,
    PresentTonemapPipeline => tonemap_pipeline,
    BloomPipelines => bloom_pipelines,
    BloomTargets => bloom_targets,
    SmaaPipelines => smaa_pipelines,
    SmaaTargets => smaa_targets,
    TransparentPipeline => transparent_pipeline,
    TransparentPasses => transparent_passes,
    ClusterBuildPass => cluster_build_pass,
    TransparentTargets => transparent_targets,
    LightClusters => light_clusters,
    LightTable => light_table,
    GBufferTargets => gbuffer,
    FramePool => frames,
    UiVertexRing => ui_ring,
    Swapchain => swapchain,
    OverlayPipeline => overlay_pipeline,
    GBufferPipeline => gbuffer_pipeline,
    CullPass => cull_pass,
    IndirectPass => indirect_pass,
    LodPass => lod_pass,
    ShadowBuildPass => shadow_build_pass,
    ShadowPipeline => shadow_pipeline,
    ShadowTargets => shadow_targets,
    InstanceTables => instance_tables,
    Uploader => uploader,
    MeshPool => pool,
    BindlessTables => tables,
    ImageCache => image_cache,
    AtlasGpu, // 由 overlay 插入，不归创建链
    GpuTimer => gpu_timer,
    Context => ctx,
}
