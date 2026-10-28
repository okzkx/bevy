//! 跟 Vulkan 设备打交道的进程级对象：上下文、交换链、内存契约与 buffer、诊断回读。

pub(in crate::render) mod context;
pub(in crate::render) mod memory;
pub(in crate::render) mod readback;
pub(in crate::render) mod swapchain;
