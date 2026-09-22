//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//! 施工记录见 `.agent/docs/2-宿主壳/`。
//!
//! 模块按生命周期分层（VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - [`vulkan`]：进程级——Entry/Instance/Surface/Device/Queue；
//! - [`swapchain`]：resize 级——swapchain + images/views，随窗口尺寸重建；
//! - [`frames`]：帧级——命令缓冲与同步对象，跨重建轮转复用；
//! - [`host`]：宿主桥——bevy 调度侧接线（init/draw_frame/teardown 三系统 + 禁渲染补位），
//!   步骤 3 开工前的结构整理中自 main.rs 迁入；
//! - [`error`]：类型化错误（OUT_OF_DATE 是控制流，不是失败）；
//! - [`syntax`]：错误处理语法糖（移植自 frenderer syntax crate，warn + 早退哲学）。

pub mod error;
pub mod frames;
pub mod host;
pub mod swapchain;
pub mod syntax;
pub mod vulkan;
