//! Vulkan 渲染层，分五个目录。底下三层依赖只朝下：`core` ← `resident` ← `passes`；
//! `frame` 在最上面把各 pass 串成一帧；`stats` 只做读数，谁都可以写它。
//!
//! | 目录 | 内容 |
//! |---|---|
//! | `core` | `device`：context、swapchain、内存与 buffer、诊断回读；`target`：颜色/深度附件；`binding`：set 布局声明、shader ABI 表、管线布局配方；`pipeline`：图形/compute/全屏管线骨架；`record`：绑定、屏障、dispatch、全屏 pass 的录制助手 |
//! | `resident` | 常驻 GPU 数据：mesh 大池、贴图缓存、上传器、bindless 描述符表、每帧 UBO、实例表、光源表 |
//! | `passes` | 渲染功能，一个功能一个模块：visibility（剔除/Hi-Z/遮挡/LOD/indirect）、gbuffer、shadow、lighting、transparent、post、overlay |
//! | `frame` | 帧槽与同步、整帧录制编排、帧率同步设置、着色调试模式 |
//! | `stats` | GPU 计时、帧样本账、监控历史、egui pass 计时 |
//!
//! 生命周期分四级：进程级（随进程活）、resize 级（随窗口尺寸重建）、资产级（随资产增删）、
//! 帧级（按帧槽轮转）；各类型属于哪一级写在它自己的文档里。
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 上层（driver 的帧编排、overlay、remote）只消费本层重出口的类型，不进子模块
//! 内部；本层不依赖 common 以外的任何顶层模块。

mod core;
mod frame;
mod passes;
mod resident;
mod stats;

pub use self::core::device::context::{Context, MAX_FRAMES_IN_FLIGHT};
pub use self::core::target::color::HDR_FORMAT;
pub use self::core::binding::set_layout::{SetLayout, SetLayouts};
pub(crate) use self::core::binding::layout_registry::{ALL_PIPELINES, ALL_SETS};
pub use passes::visibility::cull::{
    aabb_visible, frustum_planes, CullDiagnostics, CullDraw, CullPass, CullSettings,
};
pub use passes::visibility::hiz::{
    occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z, HizCensus, HizPyramid,
};
// 诊断回读助手：BRP 读数方法族（remote/）与 render 内部共用，crate 级重出口。
pub(crate) use self::core::device::readback::readback_buffer;
pub use passes::visibility::indirect::{IndirectDraw, IndirectPass, IndirectStats};
pub use passes::visibility::lod::{
    lod_hysteresis_level, lod_raw_level, LodDiagnostics, LodDraw, LodPass, LodSettings,
};
pub use passes::visibility::occlusion::{
    aabb_screen_rect, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback, OcclusionPass,
    OcclusionSettings,
};
pub use resident::bindless_texture_table::{BindlessTextureTable, TABLE_CAPACITY};
pub(crate) use resident::bindless_texture_table::SlotBinding;
pub use resident::frame_uniforms::{
    pack_frame_uniforms, FrameUniforms, FrameUniformsData, PLACEHOLDER_DIR_TO_LIGHT,
};
pub(crate) use resident::frame_uniforms::{
    FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC,
    FRAME_MODE_NORMAL, FRAME_MODE_POSITION, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS,
    FRAME_MODE_UNLIT,
};
pub use resident::images::{image_spec, sampler_key, GpuImage, ImageCache, ImageConvertError, ImageSpec, SamplerKey};
pub use resident::instance_table::{
    conservative_world_aabb, pack_lod_entry, pack_material_record, pack_model, pack_object_record,
    InstanceTables, GAP_MARK_SIZE, INDIRECT_COMMAND_SIZE, LOD_ENTRY_SIZE, LOD_LEVEL_SIZE,
    LOD_TABLE_STRIDE, MARK_SIZE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE, OBJECT_TABLE_CAPACITY,
};
pub use resident::mesh_convert::{convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE};
pub use resident::pool::{MeshPool, MeshSlot, PoolRange};
pub use resident::uploader::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, UploadBatch, Uploader,
};
pub(crate) use resident::uploader::Ticket;
pub use stats::perf::{FrameSample, GpuTimer, PerfLedger};
pub use stats::egui_pass_timing::EguiPassTiming;
pub use frame::frame_sync_settings::FrameSyncSettings;
pub(crate) use stats::monitor_history::MonitorHistory;
// 不导出 LIGHT_TYPE_SPOT：聚光灯还没实现，导出了也没人用
pub use resident::light_table::{LightRecord, LightTable, LIGHT_FLAG_SHADOW, LIGHT_TYPE_POINT};
pub use frame::render_mode::RenderMode;
pub use passes::lighting::cluster::{
    cluster_reference, pack_cluster_uniforms, ClusterBuildDraw, ClusterBuildPass, ClusterGrid,
    LightClusters,
};
pub use passes::transparent::{
    TransparentFrame, TransparentPasses, TransparentPipeline, TransparentTargets,
    TRANSPARENT_CAPACITY,
};
pub use passes::lighting::composite_pipeline::CompositePipeline;
pub use passes::post::present_tonemap_pipeline::PresentTonemapPipeline;
pub use passes::post::bloom::{
    pack_bloom_params, BloomFrameDraw, BloomPipelines, BloomParamsData, BloomSettings,
    BloomTargets, TonemapCurve,
};
pub(crate) use passes::post::bloom::BLOOM_LEVELS;
pub use passes::gbuffer::gbuffer_targets::{
    GBufferTargets, ALBEDO_FORMAT, MATPARAM_FORMAT, NORMAL_FORMAT,
};
pub use self::core::target::depth::DEPTH_FORMAT;
pub use frame::frame_pool::{Frame, FramePool};
pub use frame::frame_draw::FrameDraw;
pub use passes::overlay::overlay_pipeline::{OverlayPipeline, UiDrawCall, UiPaint};
pub use passes::overlay::atlas_gpu::AtlasGpu;
pub use passes::overlay::ui_vertex_ring::UiVertexRing;
pub(crate) use passes::overlay::ui_vertex_ring::UiVertex;
pub use passes::gbuffer::gbuffer_pipeline::GBufferPipeline;
pub use passes::shadow::shadow_map::{ShadowPipeline, ShadowTargets, ShadowWriteDraw};
pub use passes::post::smaa::{
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
};
pub use passes::shadow::shadow_build::ShadowBuildPass;
pub use passes::shadow::cascades::{
    cascade_matrices, pack_shadow_uniforms, ShadowCascadeInput,
    ShadowCascades, ShadowDiagnostics, ShadowSettings, ShadowUniformsData, SHADOW_CASCADES,
    SHADOW_MAP_SIZE, SHADOW_UBO_SIZE,
};
pub use self::core::device::memory::buffer::GpuBuffer;
pub use self::core::device::memory::contract::{align_up, MemoryContract};
pub use self::core::device::memory::role::BufferRole;
pub use self::core::device::swapchain::{AcquireOutcome, Swapchain, SWAPCHAIN_COLOR_FORMAT};
