//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//!
//! 分层（上层可依赖下层，反向不允许；整体单向无环）：
//!
//! ```text
//! main（装配：引用各层插件）
//!   driver（帧编排）──► overlay（egui 调试 UI）──┐
//!     │                 remote（BRP 远程通道）───┤
//!     └──────────────────────────────────────────┴─► scene（ECS 侧场景机制）
//!                                                     ──► render（Vulkan 资源与录制）
//!                                                           ──► common（工具层）
//! ```
//!
//! 允许的顶层依赖：driver → overlay/scene/render/common；overlay、remote →
//! scene/render/common（两者互不依赖，也不依赖 driver）；scene → render/common；
//! render → common；common 不依赖任何顶层模块。下层需要被上层"排在之后/之前"
//! 时暴露 `SystemSet`（如 `scene::ResidencySet`、`scene::PointerCaptureSet`），由
//! 上层声明排序；跨层共享的设置/读数类型下沉到被双方依赖的那层（如
//! `render::RenderMode`、`render::FrameSyncSettings`、`scene::DrawListVerify`）。
//!
//! 各层职责：
//! - [`common`]：工具层（横切、零业务依赖）——类型化错误与统一错误通道
//!   （[`common::error`]）、错误处理语法糖（[`common::syntax`]）；
//! - [`render`]：Vulkan 资源与录制——按生命周期分子目录：`context`（进程级）、
//!   `swapchain`（resize 级）、`pipeline/*`（帧编排录制、各管线与附件、UI 顶点环与
//!   图集 GPU 账面）、`assets/*`（池/上传/贴图/描述符表/GPU 常驻绘制数据）、
//!   `gpu/*`（剔除/遮挡/LOD/indirect/Hi-Z 的 compute 半边）、`shading/*`（光源表、
//!   CSM、着色模式）；另含各 GPU 特性的开关与诊断账面、帧样本账与监控读数契约；
//! - [`scene`]：ECS 侧场景机制（零 Vulkan 调用）——机制半边 `mechanism`（材质缝、
//!   稳定槽账本与变更发现、驻留管理、相机/日光控制与输入仲裁窄接口、对象/资产/
//!   Transform 写操作封装、层级树索引）+ 业务半边 `content`（进场/取景/灯光参数/
//!   进场核验）；
//! - [`overlay`]：egui 调试 UI（裸接 ash）——框架半边 `framework`（Context/字体、
//!   Update 的 egui pass、输入桥、发布 `PointerCapture`、绘制半边 `paint_overlay`）+
//!   业务半边 `tool_window`/`widgets`（工具窗口各页、坐标轴指示器、加载弹窗）；
//! - [`remote`]：BRP 远程直控通道（HTTP JSON-RPC @ 127.0.0.1:15702）——
//!   [`remote::AshRemotePlugin`] 是协议面注册单点，方法族按域分子模块；写侧复用
//!   scene 机制层的 Transform 编辑、相机给值与对象/资产操作封装，读侧直读 scene/
//!   render 的账面，与 overlay 同源而不经过 overlay；
//! - [`driver`]：帧编排——`host`（宿主桥插件：禁渲染补位 + 设置资源插入 + 系统进
//!   调度；子模块 `init` 初始化链与反序拆除、`frame` 帧循环 `draw_frame`、
//!   `drawlist` DrawList 组装、`frame_sync` 帧率同步应用）、`upload`（上传编排
//!   `flush_uploads`：Last 里排在驻留 tick 之后、draw_frame 之前）；
//! - main.rs：纯组装（装配级配置 + 插件清单；冻结边界只许增删 add_plugins）。

pub mod common;
pub mod driver;
pub mod overlay;
pub mod remote;
pub mod scene;
pub mod render;
