//! Vulkan 资源层：按生命周期与职责分层（VulkanContext字段释义：从Entry到Swapchain.md §12）。
//!
//! 目录形状（8.1 起按职责分子目录，`vulkan/` 平铺家族退役——步骤 8 §5 定案）：
//!
//! | 位置 | 生命周期 | 职责 |
//! |---|---|---|
//! | `context` | 进程级 | Entry/Instance/Surface/Device/Queue,随进程活 |
//! | `swapchain` | resize 级 | swapchain + images/views,随窗口尺寸整体重建 |
//! | `pipeline/frames` | 帧级 | 命令缓冲 + 双信号量 + fence + 深度附件,`MAX_FRAMES_IN_FLIGHT` 组轮转复用 |
//! | `pipeline/pipeline` | 进程级 | 图形管线(dynamic rendering)+ pipeline layout(双 set + push) |
//! | `pipeline/overlay_pipeline` | 进程级 | overlay 管线(set0 共享 + 16B push + 深度声明对齐读写关 + blend)+ UI 录制契约 |
//! | `pipeline/depth` | 帧级 | 深度附件本体(D32 reverse-Z,随帧槽/swapchain 重建) |
//! | `resources` | 资产级 | 内存契约:类型选择/绑定/对齐/coherent 分支 |
//! | `assets/pool` | 资产级 | 顶点/索引大池:bump 分配 + 资产身份驻留缓存 |
//! | `assets/uploader` | 批次级 | staging 环 + timeline 票据 + transfer 合批提交 |
//! | `assets/mesh_convert` | 纯函数 | `Assets<Mesh>` → 32B 交错顶点 + U32 索引 |
//! | `assets/images` | 资产级 | 贴图:bevy `Image` → VkImage/view/sampler + 驻留缓存 |
//! | `assets/descriptors` | 资产级 | 常驻描述符表(set0 双表/set1 每帧 UBO)+ 槽位发布 |
//! | `assets/instance_table` | 资产级 | GPU 常驻绘制数据(5.1):对象表/模型环/材质环 + set2 |
//! | `gpu/cull` | 进程级+纯函数 | 视锥剔除 compute 半边(5.2):CullPass 管线 + 平面提取 + CPU 参照 + 诊断账面 |
//! | `gpu/occlusion` | 进程级+纯函数 | Hi-Z 遮挡测试 compute 半边(6.1):OcclusionPass 管线 + 投屏参照 + fallback 账面 |
//! | `gpu/lod` | 进程级+纯函数 | GPU LOD 选档 compute 半边(6.2):LodPass 管线 + 判据/迟滞纯函数 + 诊断账面 |
//! | `gpu/hiz` | 进程级+纯函数 | Hi-Z 深度金字塔(6.1.2):HizPyramid ×2 帧槽 + 逐级归约管线 + reverse-Z 保守方向纯函数 |
//! | `gpu/indirect` | 进程级+纯函数 | indirect 闭环 compute 半边(5.3):IndirectPass 命令组装管线 + 诊断账面 + 命令布局参照 |
//! | `shading/light_table` | 资产级 | 光源常驻表(8.2.2):punctual 光源 storage 表 + 槽位账本 + 脏行增量重写(常驻拍板形态) |
//! | `perf` | 进程级 | GPU timestamp 计时(GpuTimer 每帧槽 query pool)+ 帧样本账(PerfLedger 分位数,5.5) |
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 编排方 [`crate::driver::host`] 只消费本层重出口的类型，不进子模块内部。

mod assets;
mod context;
mod gpu;
mod perf;
mod pipeline;
mod resources;
mod shading;
mod swapchain;

pub use context::{Context, HeapBudgetReading};
pub use gpu::cull::{
    aabb_outside_plane, aabb_visible, frustum_planes, pack_cull_push, CullDiagnostics, CullDraw,
    CullPass, CullSettings, CULL_PUSH_SIZE,
};
pub use gpu::hiz::{
    occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z, HizCensus, HizPyramid,
    HIZ_FORMAT,
};
// 诊断回读助手（8.1.3 透明族判定线③立的面）：BRP 读数方法族（remote/）与
// render 内部同频复用，crate 级重出口。
pub(crate) use gpu::hiz::readback_buffer;
pub use gpu::indirect::{IndirectDraw, IndirectPass, IndirectStats, INDIRECT_PUSH_SIZE};
pub use gpu::lod::{
    lod_hysteresis_level, lod_raw_level, pack_lod_push, LodDiagnostics, LodDraw, LodPass,
    LodSettings, LOD_PUSH_SIZE,
};
pub use gpu::occlusion::{
    aabb_screen_rect, pack_occlusion_push, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback,
    OcclusionPass, OcclusionSettings, OCCLUSION_PUSH_SIZE,
};
pub use assets::descriptors::{
    pack_frame_uniforms, BindlessTables, FrameUniformsData, SlotBinding, TABLE_CAPACITY,
    PLACEHOLDER_DIR_TO_LIGHT,
    FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC, FRAME_MODE_NORMAL,
    FRAME_MODE_POSITION, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS, FRAME_MODE_UNLIT,
};
pub use assets::images::{image_spec, sampler_key, GpuImage, ImageCache, ImageConvertError, ImageSpec, SamplerKey};
pub use assets::instance_table::{
    conservative_world_aabb, pack_lod_entry, pack_material_record, pack_model, pack_object_record,
    InstanceTables, GAP_MARK_SIZE, INDIRECT_COMMAND_SIZE, LOD_ENTRY_SIZE, LOD_LEVEL_SIZE,
    LOD_TABLE_STRIDE, MARK_SIZE, MATERIAL_RECORD_SIZE, MATERIAL_TABLE_CAPACITY, MAX_LOD_LEVELS,
    MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE, OBJECT_TABLE_CAPACITY, OCC_MARK_SIZE,
    PRESENCE_ENTRY_SIZE,
};
pub use assets::mesh_convert::{convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE};
pub use assets::pool::{MeshPool, MeshSlot, PoolRange};
pub use assets::uploader::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, Ticket, UploadBatch, Uploader,
};
pub use perf::{FrameSample, GpuTimer, PerfLedger, QUERIES_PER_FRAME};
pub use shading::{
    LightRecord, LightTable, LIGHT_FLAG_SHADOW, LIGHT_RECORD_SIZE, LIGHT_TABLE_CAPACITY,
    LIGHT_TYPE_POINT,
};
pub use pipeline::cluster::{
    cluster_grid, cluster_reference, pack_cluster_uniforms, slice_view_depth, ClusterBuildDraw,
    TransparentFrame,
    ClusterGrid, ClusterTargets, TransparentCollectDraw,
    TransparentGatherDraw, TransparentPasses, TransparentPipeline, CLUSTER_FAR, CLUSTER_NEAR,
    CLUSTER_SLICES, CLUSTER_TILE_SIZE, MAX_PER_CLUSTER, TRANSPARENT_CAPACITY,
};
pub use pipeline::fullscreen_pipeline::{CompositePipeline, PresentTonemapPipeline};
pub use pipeline::post::{
    pack_bloom_params, BloomFrameDraw, BloomPipelines, BloomParamsData, BloomSettings,
    BloomTargets, BLOOM_LEVELS,
};
pub use pipeline::gbuffer::{
    GBufferTargets, ALBEDO_FORMAT, HDR_FORMAT, MATPARAM_FORMAT, NORMAL_FORMAT,
};
pub use pipeline::depth::{DepthTarget, DEPTH_FORMAT};
pub use pipeline::frames::{FrameDraw, Frame, FramePool, MAX_FRAMES_IN_FLIGHT};
pub use pipeline::overlay_pipeline::{
    pack_ui_push, OverlayPipeline, UiDrawCall, UiPaint, UI_PUSH_CONSTANTS_SIZE, UI_VERTEX_STRIDE,
};
pub use pipeline::graphics_pipeline::GraphicsPipeline;
pub use pipeline::shadow::{ShadowPipeline, ShadowTargets, ShadowWriteDraw, SHADOW_UBO_SIZE};
pub use pipeline::smaa::{
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
    SMAA_INFO_SIZE,
};
pub use gpu::shadow_build::ShadowBuildPass;
pub use shading::shadow::{
    cascade_matrices, cascade_slice_boundaries, pack_shadow_uniforms, ShadowCascadeInput,
    ShadowDiagnostics, ShadowSettings, ShadowUniformsData, SHADOW_CASCADES, SHADOW_MAP_SIZE,
    SHADOW_MAX_DISTANCE, SHADOW_NEAR_DISTANCE,
};
pub use resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
pub use swapchain::{AcquireOutcome, PresentOutcome, Swapchain};
