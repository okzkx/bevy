//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳，
//! 负责组装禁渲染的 Bevy 并驱动 ash 清屏帧循环；本 lib 从 step3 起承接
//! ash 侧的资源上传与绘制。施工记录见 `.agent/docs/step2-宿主壳/`。
//!
//! 模块按生命周期分层（VulkanContext字段释义.md §12）：
//! - [`vulkan`]：进程级——Entry/Instance/Surface/Device/Queue；
//! - [`swapchain`]：resize 级——swapchain + images/views，随窗口尺寸重建；
//! - [`frames`]：帧级——命令缓冲与同步对象，跨重建轮转复用；
//! - [`error`]：类型化错误（OUT_OF_DATE 是控制流，不是失败）。

pub mod error;
pub mod frames;
pub mod swapchain;
pub mod vulkan;
