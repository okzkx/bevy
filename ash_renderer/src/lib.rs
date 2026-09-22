//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//! 施工记录见 `.agent/docs/2-宿主壳/`。
//!
//! 模块按职责分层（进程级→编排→资源→数据，VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - [`vulkan`]：Vulkan 资源层——按生命周期分三个子模块：`context`（进程级：
//!   Entry/Instance/Surface/Device/Queue）、`swapchain`（resize 级：随窗口尺寸重建）、
//!   `frames`（帧级：命令缓冲与同步对象，跨重建轮转复用）；
//! - [`scene`]：ECS 侧取数（step3 施工 3.1）——材质缝接线 / 场景进场 / 相机 / 灯光
//!   按场景元素各自成组，零 Vulkan 代码；
//! - [`host`]：宿主桥——bevy 调度侧接线（init/draw_frame/teardown 三系统 + 禁渲染补位），
//!   步骤 3 开工前的结构整理中自 main.rs 迁入；
//! - [`error`]：类型化错误（OUT_OF_DATE 是控制流，不是失败）；
//! - [`syntax`]：错误处理语法糖（移植自 frenderer syntax crate，warn + 早退哲学）。

pub mod error;
pub mod host;
pub mod scene;
pub mod syntax;
pub mod vulkan;
