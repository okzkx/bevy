//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//! 施工记录见 `.agent/docs/2-宿主壳/`。
//!
//! 模块按职责分层（进程级→编排→资源→数据，VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - [`vulkan`]：Vulkan 资源层——按生命周期分子模块：`context`（进程级：
//!   Entry/Instance/Surface/Device/Queue）、`swapchain`（resize 级：随窗口尺寸重建）、
//!   `frames`（帧级：命令缓冲与同步对象，跨重建轮转复用）、`resources`（资产级：
//!   内存契约,3.2.2.1 定案 memory type/usage/绑定/对齐）、`pool`（资产级:DEVICE_LOCAL
//!   大池 + bump + 驻留缓存,3.2.2.2/3.2.2.3）、`uploader`（批次级:staging 环 + timeline
//!   票据,3.2.4）、`mesh_convert`（纯函数:32B 交错转换,3.2.3）；
//! - [`scene`]：ECS 侧取数（step3 施工 3.1）——材质缝接线 / 场景进场 / 相机 / 灯光
//!   按场景元素各自成组 + 采集（PostUpdate 帧末直读产 CollectedScene 快照），零 Vulkan 代码；
//! - [`upload`]：上传编排（3.2.4 的 flush_uploads 系统）——快照去重 → 转换 → 容量
//!   保证 → 合批提交,Last 里排在 draw_frame 之前；
//! - [`host`]：宿主桥——bevy 调度侧接线（init/draw_frame/teardown 三系统 + 禁渲染补位），
//!   步骤 3 开工前的结构整理中自 main.rs 迁入；
//! - [`error`]：类型化错误（OUT_OF_DATE 是控制流，不是失败）；
//! - [`syntax`]：错误处理语法糖（移植自 frenderer syntax crate，warn + 早退哲学）。

pub mod error;
pub mod host;
pub mod scene;
pub mod syntax;
pub mod upload;
pub mod vulkan;
