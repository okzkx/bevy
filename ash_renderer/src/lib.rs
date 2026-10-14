//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//!
//! 依赖方向单向无环：
//!
//! ```text
//! main ──► driver（渲染驱动）──┬─► scene（ECS 取数：账本 + 变更发现，增量维护）
//!                              ├─► overlay（egui 调试 UI，ECS 侧）
//!                              └─► vulkan（Vulkan 资源）──► common（工具层）
//!     ──► remote（BRP 远程通道）──► scene / overlay（语义复用）
//! ```
//!
//! 模块按职责分层：
//! - [`common`]:工具层(横切,零业务依赖)——[`common::error`](类型化错误:OUT_OF_DATE
//!   是控制流不是失败)、[`common::syntax`](错误处理语法糖,frenderer syntax crate
//!   移植,warn + 早退哲学);
//! - [`vulkan`]:Vulkan 资源层——按生命周期分子模块:`context`(进程级:
//!   Entry/Instance/Surface/Device/Queue)、`swapchain`(resize 级:随窗口尺寸重建)、
//!   `frames`(帧级:命令缓冲与同步对象,跨重建轮转复用)、`resources`(资产级:
//!   内存契约:memory type/usage/绑定/对齐)、`pool`(资产级:DEVICE_LOCAL
//!   大池 + bump + 驻留缓存)、`uploader`(批次级:staging 环 + timeline
//!   票据)、`mesh_convert`(纯函数:32B 交错转换)、`images`(资产级:贴图
//!   链路)、`descriptors`(资产级:常驻描述符表 + 槽位发布)、
//!   `pipeline`(接口件:图形管线);
//! - [`scene`]:ECS 侧取数（分机制/业务两半；取数形态为"稳定槽账本 +
//!   变更发现"，非每帧全量快照）——机制半边
//!   `mechanism`(材质缝接线/账本与变更发现/宽高比补位,换场景内容不变)+ 业务半边
//!   `content`(FlightHelmet 进场/取景/灯光参数/进场核验),零 Vulkan 代码;
//! - [`overlay`]:egui 调试 UI(裸接 ash——禁渲染宿主挂不上 bevy_egui/
//!   egui-wgpu),框架/业务两半:`input`(bevy 事件→egui RawInput 输入桥)、`ui`
//!   (框架半边:Update 的 egui pass,产 EguiFrame)、`debug_window`(业务半边:
//!   帧统计窗口与 RenderMode 单选资源)、`debug_hub_window`(业务半边:调试窗口
//!   总控,各窗口显隐开关 DebugWindowsOpen)、`entity_tree_window`(业务半边:
//!   实体层级树 + 点选 SelectedEntity)、`paint`(draw_frame 内的绘制半边:
//!   图集 CPU 镜像/整传新槽/顶点环,paint_overlay→UiPaint);
//!   overlay 管线与 UI 录制契约住 `vulkan::overlay_pipeline`(`OverlayPipeline`
//!   共享 set0、`UiPaint`/`UiDrawCall` 供 `frames` 接画);
//! - [`remote`]:BRP 远程直控通道(HTTP JSON-RPC @ 127.0.0.1:15702)——AI
//!   免开 UI 查询/改值/推拉相机。框架半边在 mod 根:[`remote::AshRemotePlugin`]
//!   是协议面注册单点(装 [`bevy::remote::RemotePlugin`]+HTTP 传输+全方法族),
//!   外加 scene_tree/set_transform/set_camera 三个场景语义方法;方法族按域分子
//!   模块:[`remote::mouse`](虚拟鼠标:窗口无关输入源,注入
//!   `Messages<WindowEvent>` 与 bevy_winit 同层)、[`remote::objects`](对象
//!   生命周期方法族)、[`remote::assets`](资产内容变更 + 动态加载);
//!   读侧官方 inspection 方法族随 RemotePlugin 默认注册,写侧复用 overlay 的
//!   Transform 编辑封装与 scene 机制层的相机给值/操作封装,不养两份逻辑;
//! - [`driver`]:渲染驱动——bevy 调度侧编排,横跨 scene 与 vulkan:
//!   `host`(宿主桥插件:禁渲染补位 + 系统进调度)、`init`(Vulkan 初始化链与反序拆除)、
//!   `frame`(帧循环 draw_frame)、`upload`(上传编排:flush_uploads,Last 里 before
//!   draw_frame);
//! - main.rs:纯组装(装配级配置 + 插件清单;冻结边界只许增删 add_plugins)。

pub mod common;
pub mod driver;
pub mod overlay;
pub mod remote;
pub mod scene;
pub mod vulkan;
