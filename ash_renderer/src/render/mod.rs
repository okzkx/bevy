//! Vulkan 渲染层，分五个目录。底下三层依赖只朝下：`core` ← `resident` ← `passes`；
//! `frame` 在最上面把各 pass 串成一帧；`stats` 只做读数，谁都可以写它。
//!
//! | 目录 | 内容 |
//! |---|---|
//! | `core` | `device`：context、swapchain、内存与 buffer、诊断回读、队列传输与 timeline ticket；`target`：颜色/深度附件；`binding`：set 布局声明、shader ABI 表、管线布局配方；`pipeline`：图形/compute/全屏管线骨架；`record`：绑定、屏障、dispatch、全屏 pass 的录制助手 |
//! | `resident` | 常驻 GPU 数据：mesh 大池、贴图缓存、bindless 描述符表、每帧 UBO、实例表、光源表 |
//! | `passes` | 渲染功能，一个功能一个模块：visibility（剔除/Hi-Z/遮挡/LOD/indirect）、gbuffer、shadow、lighting、transparent、post、sky、ibl、overlay |
//! | `frame` | 帧槽与同步、整帧录制编排、帧率同步设置、着色调试模式 |
//! | `stats` | GPU 计时、帧样本账、监控历史、egui pass 计时 |
//!
//! 生命周期分四级：进程级（随进程活）、resize 级（随窗口尺寸重建）、资产级（随资产增删）、
//! 帧级（按帧槽轮转）；各类型属于哪一级写在它自己的文档里。
//!
//! `layout_registry` 登记全部管线/set 声明，要引用 `passes` 与 `resident`，所以放在本层
//! 根上而不是 `core`；布局契约测试跟着它。
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 上层（driver 的帧编排、overlay、remote）只消费本层重出口的类型，不进子模块
//! 内部；本层不依赖 common 以外的任何顶层模块。

mod core;
mod frame;
mod layout_registry;
mod passes;
mod ray_tracing;
mod resident;
mod stats;

#[cfg(test)]
mod layout_contract_tests;

pub use self::core::binding::set_layout::{SetLayout, SetLayouts};
pub use self::core::device::context::{Context, MAX_FRAMES_IN_FLIGHT};
pub use self::core::device::ray_tracing::{RayTracingCaps, RayTracingSupport};
pub use self::core::target::color::HDR_FORMAT;
pub use self::ray_tracing::{BlasRegistry, RayScene, RaySceneStats, RayTracingSettings, TlasFrame};
pub(crate) use layout_registry::{ALL_PIPELINES, ALL_SETS};
pub use passes::visibility::cull::{
    aabb_visible, frustum_planes, CullDiagnostics, CullDraw, CullPass, CullSettings,
};
pub use passes::visibility::hiz::{
    occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z, HizCensus, HizPyramid,
};
pub use passes::visibility::hiz_frame::HizFrameDraw;
// 诊断回读助手：BRP 读数方法族（remote/）与 render 内部共用，crate 级重出口。
pub(crate) use self::core::device::readback::{
    readback_buffer, readback_image_region, OneShotReadback,
};
pub(crate) use core::device::transfer::{block_bytes, Ticket};
pub use core::device::transfer::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, UploadBatch, Uploader,
};
pub use frame::frame_sync_settings::FrameSyncSettings;
pub use passes::visibility::indirect::{IndirectDraw, IndirectPass, IndirectStats};
pub use passes::visibility::lod::{
    lod_hysteresis_level, lod_raw_level, LodDiagnostics, LodDraw, LodPass, LodSettings,
};
pub use passes::visibility::occlusion::{
    aabb_screen_rect, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback, OcclusionPass,
    OcclusionSettings,
};
pub(crate) use resident::assets::bcn::{bake, has_alpha, BakeError, BakedBcn, TextureRole};
pub(crate) use resident::assets::bcn_cache::{cache_key, BcnCache};
pub use resident::assets::image_spec::{image_spec, ImageSpec, ImageSpecError};
pub use resident::assets::images::{GpuImage, ImageCache};
pub use resident::assets::mesh_convert::{
    convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE,
};
pub use resident::assets::pool::{MeshPool, MeshSlot, PoolRange};
pub use resident::sampler::{sampler_key, SamplerKey};
pub(crate) use resident::tables::bindless_texture_table::SlotBinding;
pub use resident::tables::bindless_texture_table::{BindlessTextureTable, TABLE_CAPACITY};
pub use resident::tables::frame_uniform_table::{
    pack_frame_uniforms, FrameUniformTable, FrameUniformsData, PLACEHOLDER_DIR_TO_LIGHT,
};
pub(crate) use resident::tables::frame_uniform_table::{
    FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_IBL_DIFFUSE, FRAME_MODE_IBL_SPECULAR,
    FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC, FRAME_MODE_NORMAL, FRAME_MODE_POSITION,
    FRAME_MODE_PROBE_BUCKETS, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS, FRAME_MODE_UNLIT,
    FRAME_UBO_SIZE,
};
pub use resident::tables::instance_table::{
    conservative_world_aabb, material_alpha_fields, material_is_transparent, pack_lod_entry,
    pack_material_record, pack_model, pack_object_record, InstanceTables, MaterialAlpha,
    ALPHA_MODE_BLEND, ALPHA_MODE_MASK, ALPHA_MODE_OPAQUE, BLAS_ADDRESS_SIZE, GAP_MARK_SIZE,
    INDIRECT_COMMAND_SIZE, LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE, MARK_SIZE,
    MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS, MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE,
    OBJECT_TABLE_CAPACITY, TLAS_INSTANCE_SIZE,
};
pub use stats::egui_pass_timing::EguiPassTiming;
pub(crate) use stats::monitor_history::MonitorHistory;
pub use stats::perf::{FrameSample, GpuTimer, PerfLedger};
// 不导出 LIGHT_TYPE_SPOT：聚光灯还没实现，导出了也没人用
pub use self::core::binding::set_layout::DescSet;
pub use self::core::device::memory::buffer::GpuBuffer;
pub use self::core::device::memory::contract::{align_up, MemoryContract};
pub use self::core::device::memory::role::BufferRole;
pub use self::core::device::swapchain::{
    AcquireOutcome, PresentTarget, Swapchain, SWAPCHAIN_COLOR_FORMAT,
};
pub use self::core::target::depth::DEPTH_FORMAT;
pub use frame::frame_pool::{Frame, FramePool, UploadWait};
pub use frame::render_features::RenderFeatures;
pub use frame::render_mode::RenderMode;
pub use frame::scene_resources::SceneResources;
pub use passes::gbuffer::gbuffer_pipeline::GBufferPipeline;
pub use passes::gbuffer::gbuffer_targets::{
    GBufferTargets, ALBEDO_FORMAT, EMISSIVE_FORMAT, MATPARAM_FORMAT, NORMAL_FORMAT,
};
pub use passes::gi::allocation::{
    alloc_grid, eviction_groups, pack_alloc_push, scrolling_groups, GiAllocDraw, GI_ALLOC_PUSH_SIZE,
};
pub use passes::gi::estimation::{
    bounce_alloc_groups, emissive_build_groups, estimation_groups, light_sampling_groups,
    pack_emissive_build_push, pack_light_sampling_push, plan_estimation_budget, EstimationBudget,
    GiBounceAllocDraw, GiEmissiveBuildDraw, GiEstimationDraw, GiLightSampleDraw,
    GI_EMISSIVE_BUILD_PUSH_SIZE, GI_LIGHT_SAMPLING_PUSH_SIZE, RAYS_PER_SAMPLE_WORST,
};
pub use passes::gi::filtering::{
    pack_spatial_filter_push, pack_temporal_filter_push, spatial_filter_groups,
    GiSpatialFilterDraw, GiTemporalFilterDraw, GI_SPATIAL_FILTER_PUSH_SIZE,
    GI_TEMPORAL_FILTER_PUSH_SIZE,
};
pub use passes::gi::screen_gather::{
    low_extent as scgi_low_extent, pack_denoise_push, pack_screen_gather_push, pack_upsample_push,
    upsample_groups, GiDenoiseDraw, GiScreenGatherDraw, GiUpsampleDraw, ScgiTargets, DENOISE_STEPS,
    GI_DENOISE_PUSH_SIZE, GI_SCREEN_GATHER_PUSH_SIZE, GI_UPSAMPLE_PUSH_SIZE,
    SCGI_IRRADIANCE_FORMAT, SCGI_L0_FORMAT, SCGI_L1_FORMAT,
};
pub use passes::gi::view::{pack_view_push, GiViewDraw, GI_VIEW_PUSH_SIZE};
pub use passes::gi::volume::{
    cascade_offsets as gi_cascade_offsets, cell_bucket as gi_cell_bucket,
    cell_cascade as gi_cell_cascade, cell_index as gi_cell_index, cell_storage as gi_cell_storage,
    direction_bucket as gi_direction_bucket, heartbeat_age as gi_heartbeat_age,
    is_inside_cascade as gi_is_inside_cascade, pos_mod as gi_pos_mod,
    resolve_cascade as gi_resolve_cascade, storage_to_volume as gi_storage_to_volume,
    volume_position as gi_volume_position, volume_to_storage as gi_volume_to_storage,
    volume_world_center as gi_volume_world_center, voxel_size as gi_voxel_size, BUCKETS_PER_CELL,
    BUCKET_RESOLUTION, INVALID_PATCH, MAX_CASCADES,
};
pub(crate) use passes::gi::volume::{
    flat_normal as gi_flat_normal, world_from_depth as gi_world_from_depth,
};
pub use passes::gi::{
    GiFrameDraw, GiPipelines, GiSettings, SurfaceCacheConfig, EVICT_AGE, UPDATE_MAX,
};
pub(crate) use passes::ibl::octahedral::{
    bucket_to_direction, direction_to_bucket, eval_probe_bucket,
};
pub use passes::ibl::IBL_MODE_PROBE16;
pub use passes::ibl::IBL_MODE_SH_L2;
pub use passes::ibl::{
    half_to_f32, IblBakeDraw, IblBakeInput, IblBakePlan, IblEnvironment, IblFrameDraw, IblSettings,
    IBL_CUBE_MIPS, IBL_CUBE_SIZE, IBL_MAX_MIP, IBL_PROBE_BUCKETS, IBL_PROBE_BUFFER_SIZE,
    IBL_PROBE_RESOLUTION, IBL_SH_BUFFER_SIZE, IBL_STATS_BUFFER_SIZE, IBL_STATS_FLOATS,
};
pub use passes::lighting::cluster::{
    cluster_reference, pack_cluster_uniforms, ClusterBuildDraw, ClusterBuildPass, ClusterFrameDraw,
    ClusterGrid, LightClusters,
};
pub use passes::lighting::composite_pipeline::CompositePipeline;
pub use passes::overlay::atlas_gpu::AtlasGpu;
pub use passes::overlay::overlay_pipeline::{OverlayPipeline, UiDraw, UiDrawCall, UiPaint};
pub(crate) use passes::overlay::ui_vertex_ring::UiVertex;
pub use passes::overlay::ui_vertex_ring::UiVertexRing;
pub use passes::post::bloom::{
    pack_bloom_params, BloomFrameDraw, BloomParamsData, BloomPipelines, BloomSettings,
    BloomTargets, TonemapCurve,
};
pub use passes::post::color_scope::{
    ColorScopeBindings, ColorScopeCounts, ColorScopeDomain, ColorScopeDraw, ColorScopePipelines,
    ColorScopeRatios, ColorScopeReader, ColorScopeSettings, ColorScopeSnapshot, ColorScopeTargets,
    COLOR_SCOPE_AXIS_MIN_EV, COLOR_SCOPE_AXIS_SPAN_EV, COLOR_SCOPE_BINS_BYTES,
    COLOR_SCOPE_BINS_U32, COLOR_SCOPE_CBCR_EDGE, COLOR_SCOPE_DOMAIN_U32, COLOR_SCOPE_HEADER_FRAME,
    COLOR_SCOPE_HEADER_HEIGHT, COLOR_SCOPE_HEADER_U32, COLOR_SCOPE_HEADER_WIDTH, COLOR_SCOPE_MAGIC,
    COLOR_SCOPE_STRIDE,
};
pub use passes::post::exposure::{
    pack_exposure_params, ExposureDraw, ExposureMode, ExposureParamsData, ExposurePipelines,
    ExposureSettings, ExposureTargets, EXPOSURE_TILES_X, EXPOSURE_TILES_Y,
};
pub use passes::post::post_chain::PostChainDraw;
pub use passes::post::present_tonemap_pipeline::PresentTonemapPipeline;
pub use passes::post::smaa::LDR_FORMAT;
pub use passes::post::smaa::{
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
};
pub use passes::ray_tracing::{
    ray_debug_draw, readback_debug_image, tlas_build_draw, RayTracingFrameDraw,
    RayTracingPipelines, RayTracingTargets,
};
pub use passes::shadow::cascades::{
    cascade_matrices, pack_shadow_uniforms, ShadowCascadeInput, ShadowCascades, ShadowSettings,
    ShadowUniformsData, SHADOW_CASCADES, SHADOW_MAP_SIZE, SHADOW_UBO_SIZE,
};
pub use passes::shadow::shadow_build::ShadowBuildPass;
pub use passes::shadow::shadow_frame::{ShadowFrameDraw, ShadowPasses};
pub use passes::shadow::shadow_map::{ShadowPipeline, ShadowTargets, ShadowWriteDraw};
pub use passes::sky::SkySettings;
pub use passes::transparent::{
    TransparentFrame, TransparentPasses, TransparentPipeline, TransparentTargets,
    TRANSPARENT_CAPACITY,
};
pub use resident::tables::light_table::{
    LightRecord, LightTable, LIGHT_FLAG_SHADOW, LIGHT_TYPE_POINT,
};
pub use resident::tables::surface_cache::{
    pack_surface_cache_uniforms, GiSetLayout, SurfaceCacheMemory, SurfaceCacheTables,
    SurfaceCacheUniformsData, COUNTER_ALLOC, COUNTER_DEFRAG_MOVED, COUNTER_EMISSIVE_TRIANGLES,
    COUNTER_EST_BOUNCE_HITS, COUNTER_EST_BOUNCE_MISSES, COUNTER_EST_BOUNCE_REQUESTS,
    COUNTER_EST_HITS, COUNTER_EST_PATCHES, COUNTER_EST_RAYS, COUNTER_EST_SAMPLES, COUNTER_FAILED,
    COUNTER_FREED, COUNTER_VIEW_COVERED, COUNTER_VIEW_DEVIATION, COUNTER_VIEW_HOLES,
    GI_BOUNCE_REQUEST_CAPACITY, GI_BOUNCE_REQUEST_SIZE, GI_COUNTERS_SIZE, GI_COUNTER_WORDS,
    GI_EMISSIVE_CAPACITY, GI_EMISSIVE_TRIANGLE_SIZE, GI_LIGHT_CAPACITY, GI_LIGHT_SAMPLE_CAPACITY,
    GI_LIGHT_SAMPLE_SIZE, GI_UBO_SIZE, PATCH_GEOMETRY_SIZE, PATCH_IRRADIANCE_SIZE,
    PATCH_STATISTICS_SIZE, RING_ACTIVE, RING_CONFIGS, RING_CONFIG_SIZE, RING_CONFIG_WORDS,
    RING_FLIP, RING_WATERMARK,
};
