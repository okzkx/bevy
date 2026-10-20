//! Vulkan 资源层：按生命周期分层（VulkanContext字段释义：从Entry到Swapchain.md §12）。
//!
//! | 子模块 | 生命周期 | 职责 |
//! |---|---|---|
//! | `context` | 进程级 | Entry/Instance/Surface/Device/Queue,随进程活 |
//! | `swapchain` | resize 级 | swapchain + images/views,随窗口尺寸整体重建 |
//! | `frames` | 帧级 | 命令缓冲 + 双信号量 + fence + 深度附件,`MAX_FRAMES_IN_FLIGHT` 组轮转复用 |
//! | `pipeline` | 进程级 | 图形管线(dynamic rendering)+ pipeline layout(双 set + push) |
//! | `overlay_pipeline` | 进程级 | overlay 管线(set0 共享 + 16B push + 深度声明对齐读写关 + blend)+ UI 录制契约 |
//! | `resources` | 资产级 | 内存契约:类型选择/绑定/对齐/coherent 分支 |
//! | `pool` | 资产级 | 顶点/索引大池:bump 分配 + 资产身份驻留缓存 |
//! | `uploader` | 批次级 | staging 环 + timeline 票据 + transfer 合批提交 |
//! | `mesh_convert` | 纯函数 | `Assets<Mesh>` → 32B 交错顶点 + U32 索引 |
//! | `images` | 资产级 | 贴图:bevy `Image` → VkImage/view/sampler + 驻留缓存 |
//! | `descriptors` | 资产级 | 常驻描述符表(set0 双表/set1 每帧 UBO)+ 槽位发布 |
//! | `cull` | 进程级+纯函数 | 视锥剔除 compute 半边(5.2):CullPass 管线 + 平面提取 + CPU 参照 + 诊断账面 |
//! | `occlusion` | 进程级+纯函数 | Hi-Z 遮挡测试 compute 半边(6.1):OcclusionPass 管线 + 投屏参照 + fallback 账面 |
//! | `lod` | 进程级+纯函数 | GPU LOD 选档 compute 半边(6.2):LodPass 管线 + 判据/迟滞纯函数 + 诊断账面 |
//! | `hiz` | 进程级+纯函数 | Hi-Z 深度金字塔(6.1.2):HizPyramid ×2 帧槽 + 逐级归约管线 + reverse-Z 保守方向纯函数 |
//! | `indirect` | 进程级+纯函数 | indirect 闭环 compute 半边(5.3):IndirectPass 命令组装管线 + 诊断账面 + 命令布局参照 |
//! | `perf` | 进程级 | GPU timestamp 计时(GpuTimer 每帧槽 query pool)+ 帧样本账(PerfLedger 分位数,5.5) |
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 编排方 [`crate::driver::host`] 只消费本层重出口的类型，不进子模块内部。

mod context;
mod cull;
mod depth;
mod descriptors;
mod frames;
mod hiz;
mod images;
mod indirect;
mod instance_table;
mod lod;
mod mesh_convert;
mod occlusion;
mod overlay_pipeline;
mod perf;
mod pipeline;
mod pool;
mod resources;
mod swapchain;
mod uploader;

pub use context::Context;
pub use cull::{
    aabb_outside_plane, aabb_visible, frustum_planes, pack_cull_push, CullDiagnostics, CullDraw,
    CullPass, CullSettings, CULL_PUSH_SIZE,
};
pub use descriptors::{
    pack_frame_uniforms, BindlessTables, FrameUniformsData, SlotBinding, TABLE_CAPACITY,
    PLACEHOLDER_DIR_TO_LIGHT,
    FRAME_MODE_LAMBERT, FRAME_MODE_NORMAL, FRAME_MODE_UNLIT,
};
pub use frames::{DepthTarget, FrameDraw, Frame, FramePool, DEPTH_FORMAT, MAX_FRAMES_IN_FLIGHT};pub use hiz::{
    occluded_reverse_z, occlusion_mip_level, reduce_tile_reverse_z, HizCensus, HizPyramid,
    HIZ_FORMAT,
};pub use images::{image_spec, sampler_key, GpuImage, ImageCache, ImageConvertError, ImageSpec, SamplerKey};
pub use indirect::{IndirectDraw, IndirectPass, IndirectStats, INDIRECT_PUSH_SIZE};
pub use instance_table::{
    conservative_world_aabb, pack_lod_entry, pack_material_record, pack_model, pack_object_record,
    InstanceTables, INDIRECT_COMMAND_SIZE, LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE,
    MARK_SIZE, MATERIAL_RECORD_SIZE, MATERIAL_TABLE_CAPACITY, MAX_LOD_LEVELS, MODEL_STRIDE,
    OBJECT_RECORD_SIZE, OBJECT_TABLE_CAPACITY, OCC_MARK_SIZE, PRESENCE_ENTRY_SIZE,
};
pub use lod::{
    lod_hysteresis_level, lod_raw_level, pack_lod_push, LodDiagnostics, LodDraw, LodPass,
    LodSettings, LOD_PUSH_SIZE,
};
pub use occlusion::{
    aabb_screen_rect, pack_occlusion_push, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback,
    OcclusionPass, OcclusionSettings, OCCLUSION_PUSH_SIZE,
};
pub use mesh_convert::{convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE};
pub use overlay_pipeline::{
    pack_ui_push, OverlayPipeline, UiDrawCall, UiPaint, UI_PUSH_CONSTANTS_SIZE, UI_VERTEX_STRIDE,
};
pub use perf::{FrameSample, GpuTimer, PerfLedger, QUERIES_PER_FRAME};
pub use pipeline::GraphicsPipeline;
pub use pool::{MeshPool, MeshSlot, PoolRange};
pub use resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
pub use swapchain::{AcquireOutcome, PresentOutcome, Swapchain};
pub use uploader::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, Ticket, UploadBatch, Uploader,
};
