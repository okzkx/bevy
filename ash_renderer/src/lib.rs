//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳，
//! 负责组装禁渲染的 Bevy 并驱动 ash 清屏循环；本 lib 从 step3 起承接
//! ash 侧的资源上传与绘制。施工记录见 `.agent/docs/step2-宿主壳/`。
