//! Vulkan 资源层：按生命周期与职责分层（VulkanContext字段释义：从Entry到Swapchain.md §12）。
//!
//! 目录形状（按职责分子目录）：
//!
//! | 位置 | 生命周期 | 职责 |
//! |---|---|---|
//! | `core/context` | 进程级 | Entry/Instance/Surface/Device/Queue,随进程活 |
//! | `core/swapchain` | resize 级 | swapchain + images/views,随窗口尺寸整体重建 |
//! | `core/resources` | 资产级 | 内存契约:类型选择/绑定/对齐/coherent 分支 |
//! | `core/barrier` | 纯函数 | 图像布局屏障录制助手(单 mip/单 layer,可附全局内存屏障) |
//! | `core/readback` | 调用级 | 诊断回读:一次性 staging + 命令池 + fence 的同步回读(BRP 读数族与 render 内部共用) |
//! | `stats/perf` | 进程级 | GPU timestamp 计时(GpuTimer 每帧槽 query pool)+ 帧样本账(PerfLedger 分位数) |
//! | `stats/monitor_history` | 进程级 | 持续监控历史账(fps/进程与系统内存/显存堆级预算采样点,overlay 采样、BRP 与监控页同读) |
//! | `stats/egui_pass_timing` | 进程级 | egui pass 自计时读数(overlay 写、BRP 读) |
//! | `stats/frame_sync_settings` | 进程级 | 帧率同步设置契约(开关 + 主屏刷新率读数,driver 应用、UI/BRP 读写) |
//! | `pipeline/frames` | 帧级 | 命令缓冲 + 双信号量 + fence,`MAX_FRAMES_IN_FLIGHT` 组轮转复用;帧录制编排 |
//! | `pipeline/base` | 进程级 | 管线公共件:图形管线句柄与录制助手、全屏管线骨架、颜色/深度目标、storage 描述符集 |
//! | `pipeline/geometry` | resize 级+进程级 | G-buffer 目标与写入管线、透明层(透明命令链 + 三条 compute + 透明前向管线) |
//! | `pipeline/lighting` | resize 级+进程级 | 光源分簇(cluster 表 + cluster_build)、CSM 阴影、延迟光照合成管线 |
//! | `pipeline/post` | resize 级 | Bloom、present tonemap、SMAA 1x |
//! | `pipeline/overlay` | 帧级+资产级 | overlay 管线 + UI 录制契约、字体图集 GPU 账面、UI 顶点环 |
//! | `assets/pool` | 资产级 | 顶点/索引大池:bump 分配 + 资产身份驻留缓存 |
//! | `assets/uploader` | 批次级 | staging 环 + timeline 票据 + transfer 合批提交 |
//! | `assets/mesh_convert` | 纯函数 | `Assets<Mesh>` → 32B 交错顶点 + U32 索引 |
//! | `assets/images` | 资产级 | 贴图:bevy `Image` → VkImage/view/sampler + 驻留缓存 |
//! | `assets/descriptors` | 资产级 | 常驻描述符表(set0 双表/set1 每帧 UBO)+ 槽位发布 |
//! | `assets/instance_table` | 资产级 | GPU 常驻绘制数据:对象表/模型环/材质环 + set2 |
//! | `gpu/compute` | 进程级 | compute 管线标准件:ComputePipeline + 录制句柄/交接屏障助手 |
//! | `gpu/cull` | 进程级+纯函数 | 视锥剔除 compute 半边:CullPass 管线 + 平面提取 + CPU 参照 + 诊断账面 |
//! | `gpu/occlusion` | 进程级+纯函数 | Hi-Z 遮挡测试 compute 半边:OcclusionPass 管线 + 投屏参照 + fallback 账面 |
//! | `gpu/lod` | 进程级+纯函数 | GPU LOD 选档 compute 半边:LodPass 管线 + 判据/迟滞纯函数 + 诊断账面 |
//! | `gpu/hiz` | 进程级+纯函数 | Hi-Z 深度金字塔:HizPyramid ×2 帧槽 + 逐级归约管线 + reverse-Z 保守方向纯函数 |
//! | `gpu/indirect` | 进程级+纯函数 | indirect 闭环 compute 半边:IndirectPass 命令组装管线 + 诊断账面 + 命令布局参照 |
//! | `gpu/shadow_build` | 进程级 | shadow 命令组装 compute 半边:ShadowBuildPass 管线 + 录制 |
//! | `shading/light_table` | 资产级 | 光源常驻表:punctual 光源 storage 表 + 槽位账本 + 脏行增量重写 |
//! | `shading/shadow` | 帧级+纯函数 | CSM 宿主半边:级联矩阵 + shadow UBO 打包 + 开关与诊断账面 |
//! | `shading/render_mode` | 进程级 | 着色模式(帧 UBO `mode` 的宿主侧形态)+ 唯一名称表 |
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 上层（driver 的帧编排、overlay、remote）只消费本层重出口的类型，不进子模块
//! 内部；本层不依赖 common 以外的任何顶层模块。

mod assets;
mod core;
mod gpu;
mod pipeline;
mod shading;
mod stats;

pub use self::core::context::Context;
pub use gpu::cull::{
    aabb_visible, frustum_planes, CullDiagnostics, CullDraw, CullPass, CullSettings,
};
pub use gpu::hiz::{
    occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z, HizCensus, HizPyramid,
};
// 诊断回读助手：BRP 读数方法族（remote/）与 render 内部共用，crate 级重出口。
pub(crate) use self::core::readback::readback_buffer;
pub use gpu::indirect::{IndirectDraw, IndirectPass, IndirectStats};
pub use gpu::lod::{
    lod_hysteresis_level, lod_raw_level, LodDiagnostics, LodDraw, LodPass, LodSettings,
};
pub use gpu::occlusion::{
    aabb_screen_rect, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback, OcclusionPass,
    OcclusionSettings,
};
pub use assets::descriptors::{
    pack_frame_uniforms, BindlessTables, FrameUniformsData, TABLE_CAPACITY,
    PLACEHOLDER_DIR_TO_LIGHT,
};
pub(crate) use assets::descriptors::{
    SlotBinding, FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC,
    FRAME_MODE_NORMAL, FRAME_MODE_POSITION, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS,
    FRAME_MODE_UNLIT,
};
pub use assets::images::{image_spec, sampler_key, GpuImage, ImageCache, ImageConvertError, ImageSpec, SamplerKey};
pub use assets::instance_table::{
    conservative_world_aabb, pack_lod_entry, pack_material_record, pack_model, pack_object_record,
    InstanceTables, GAP_MARK_SIZE, INDIRECT_COMMAND_SIZE, LOD_ENTRY_SIZE, LOD_LEVEL_SIZE,
    LOD_TABLE_STRIDE, MARK_SIZE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE, OBJECT_TABLE_CAPACITY,
};
pub use assets::mesh_convert::{convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE};
pub use assets::pool::{MeshPool, MeshSlot, PoolRange};
pub use assets::uploader::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, UploadBatch, Uploader,
};
pub(crate) use assets::uploader::Ticket;
pub use stats::perf::{FrameSample, GpuTimer, PerfLedger};
pub use stats::egui_pass_timing::EguiPassTiming;
pub use stats::frame_sync_settings::FrameSyncSettings;
pub(crate) use stats::monitor_history::MonitorHistory;
pub use shading::{LightRecord, LightTable, RenderMode, LIGHT_FLAG_SHADOW, LIGHT_TYPE_POINT};
pub use pipeline::lighting::cluster::{
    cluster_reference, pack_cluster_uniforms, ClusterBuildDraw, ClusterBuildPass, ClusterGrid,
    LightClusters,
};
pub use pipeline::geometry::transparent::{
    TransparentFrame, TransparentPasses, TransparentPipeline, TransparentTargets,
    TRANSPARENT_CAPACITY,
};
pub use pipeline::lighting::composite_pipeline::CompositePipeline;
pub use pipeline::post::present_tonemap_pipeline::PresentTonemapPipeline;
pub use pipeline::post::bloom::{
    pack_bloom_params, BloomFrameDraw, BloomPipelines, BloomParamsData, BloomSettings,
    BloomTargets, TonemapCurve,
};
pub(crate) use pipeline::post::bloom::BLOOM_LEVELS;
pub use pipeline::geometry::gbuffer::{
    GBufferTargets, ALBEDO_FORMAT, HDR_FORMAT, MATPARAM_FORMAT, NORMAL_FORMAT,
};
pub use pipeline::base::depth_target::DEPTH_FORMAT;
pub use pipeline::frames::{FrameDraw, Frame, FramePool, MAX_FRAMES_IN_FLIGHT};
pub use pipeline::overlay::overlay_pipeline::{OverlayPipeline, UiDrawCall, UiPaint};
pub use pipeline::overlay::atlas_gpu::AtlasGpu;
pub use pipeline::overlay::ui_vertex_ring::UiVertexRing;
pub(crate) use pipeline::overlay::ui_vertex_ring::UiVertex;
pub use pipeline::geometry::gbuffer_pipeline::GBufferPipeline;
pub use pipeline::lighting::shadow::{ShadowPipeline, ShadowTargets, ShadowWriteDraw};
pub use pipeline::post::smaa::{
    SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings, SmaaTargets,
};
pub use gpu::shadow_build::ShadowBuildPass;
pub use shading::shadow::{
    cascade_matrices, pack_shadow_uniforms, ShadowCascadeInput,
    ShadowCascades, ShadowDiagnostics, ShadowSettings, ShadowUniformsData, SHADOW_CASCADES,
    SHADOW_MAP_SIZE, SHADOW_UBO_SIZE,
};
pub use self::core::resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
pub use self::core::swapchain::{AcquireOutcome, Swapchain};
