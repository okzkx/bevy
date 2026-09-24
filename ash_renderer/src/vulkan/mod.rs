//! Vulkan 资源层：按生命周期分三个子模块（VulkanContext字段释义：从Entry到Swapchain.md §12）。
//!
//! | 子模块 | 生命周期 | 职责 |
//! |---|---|---|
//! | `context` | 进程级 | Entry/Instance/Surface/Device/Queue，随进程活 |
//! | `swapchain` | resize 级 | swapchain + images/views，随窗口尺寸整体重建 |
//! | `frames` | 帧级 | 命令缓冲 + 双信号量 + fence，`MAX_FRAMES_IN_FLIGHT` 组轮转复用 |
//! | `resources` | 资产级 | 内存契约(3.2.2.1):类型选择/绑定/对齐/coherent 分支;池归 3.2.2.2 |
//!
//! 拆分点：同步对象不挂任何一张 swapchain image 上——重建 swapchain 时原地不动；
//! 编排方 [`crate::host`] 只消费本层重出口的类型，不进子模块内部。

mod context;
mod frames;
mod resources;
mod swapchain;

pub use context::Context;
pub use frames::{Frame, FramePool, MAX_FRAMES_IN_FLIGHT};
pub use resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
pub use swapchain::{AcquireOutcome, PresentOutcome, Swapchain};
