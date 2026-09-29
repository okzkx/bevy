//! Vulkan 资源层：按生命周期分三个子模块（VulkanContext字段释义：从Entry到Swapchain.md §12）。
//!
//! | 子模块 | 生命周期 | 职责 |
//! |---|---|---|
//! | `context` | 进程级 | Entry/Instance/Surface/Device/Queue,随进程活 |
//! | `swapchain` | resize 级 | swapchain + images/views,随窗口尺寸整体重建 |
//! | `frames` | 帧级 | 命令缓冲 + 双信号量 + fence + 深度附件,`MAX_FRAMES_IN_FLIGHT` 组轮转复用 |
//! | `pipeline` | 进程级 | 3.4 图形管线(dynamic rendering)+ pipeline layout(双 set + push) |
//! | `resources` | 资产级 | 内存契约(3.2.2.1):类型选择/绑定/对齐/coherent 分支 |
//! | `pool` | 资产级 | 顶点/索引大池:bump 分配 + 资产身份驻留缓存(3.2.2.2/3.2.2.3) |
//! | `uploader` | 批次级 | staging 环 + timeline 票据 + transfer 合批提交(3.2.4) |
//! | `mesh_convert` | 纯函数 | `Assets<Mesh>` → 32B 交错顶点 + U32 索引(3.2.3) |
//! | `images` | 资产级 | 贴图:bevy `Image` → VkImage/view/sampler + 驻留缓存(3.3.1) |
//! | `descriptors` | 资产级 | 常驻描述符表(set0 双表/set1 每帧 UBO)+ 槽位发布(3.3.2~3.3.4) |
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 编排方 [`crate::host`] 只消费本层重出口的类型，不进子模块内部。

mod context;
mod descriptors;
mod frames;
mod images;
mod mesh_convert;
mod pipeline;
mod pool;
mod resources;
mod swapchain;
mod uploader;

pub use context::Context;
pub use descriptors::{BindlessTables, SlotBinding, TABLE_CAPACITY};
pub use frames::{DepthTarget, DrawCall, FrameDraw, Frame, FramePool, DEPTH_FORMAT, MAX_FRAMES_IN_FLIGHT};
pub use images::{image_spec, sampler_key, GpuImage, ImageCache, ImageConvertError, ImageSpec, SamplerKey};
pub use mesh_convert::{convert_mesh, ConvertedMesh, MeshConvertError, VERTEX_STRIDE};
pub use pipeline::{pack_push, GraphicsPipeline, PushData, PUSH_CONSTANTS_SIZE};
pub use pool::{MeshPool, MeshSlot, PoolRange};
pub use resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
pub use swapchain::{AcquireOutcome, PresentOutcome, Swapchain};
pub use uploader::{
    CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, Ticket, UploadBatch, Uploader,
};
