//! Vulkan 地基：设备上下文、交换链、内存契约与 buffer、图像屏障助手、诊断回读。
//! `render` 下其余子目录都建立在这一层之上。

pub(super) mod barrier;
pub(super) mod context;
pub(super) mod readback;
pub(super) mod resources;
pub(super) mod swapchain;
