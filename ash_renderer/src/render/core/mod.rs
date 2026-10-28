//! Vulkan 地基与管线标准件：设备上下文、交换链、内存与 buffer、图像屏障、诊断回读，
//! 以及各 pass 共用的建管线件（图形管线句柄、compute 管线、全屏管线骨架）和
//! 颜色/深度目标、storage 描述符集。`render` 下其余目录都建立在这一层之上。

pub(super) mod barrier;
pub(super) mod color_target;
pub(super) mod compute_pipeline;
pub(super) mod context;
pub(super) mod depth_target;
pub(super) mod fullscreen_pipeline;
pub(super) mod pipeline_handle;
pub(super) mod readback;
pub(super) mod resources;
pub(super) mod storage_sets;
pub(super) mod swapchain;
