//! 设备级 Vulkan 服务：上下文、交换链、内存契约与 buffer、诊断回读和队列传输。

pub(in crate::render) mod context;
pub(in crate::render) mod memory;
pub(in crate::render) mod readback;
pub(in crate::render) mod swapchain;
pub(in crate::render) mod transfer;
