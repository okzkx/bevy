//! 自研 ash Vulkan 渲染器（lib 部分）。
//!
//! 本 crate 是学习项目的渲染器落位：bin 目标（`src/main.rs`）为宿主壳入口，只做统筹
//! （组装插件后交出调度，零 Vulkan 调用）；Vulkan 编排与资源全在本 lib。
//! 施工记录见 `.agents/docs/2-宿主壳/`；3.6 分层整理见
//! `.agents/docs/3-静态取数链路/3.6-代码全量整理/`。
//!
//! 依赖方向单向无环（3.6.1 分层定案）：
//!
//! ```text
//! main ──► driver（渲染驱动）──┬─► scene（ECS 取数，读 CollectedScene 快照）
//!                              └─► vulkan（Vulkan 资源）──► common（工具层）
//! ```
//!
//! 模块按职责分四层：
//! - [`common`]:工具层(横切,零业务依赖)——[`common::error`](类型化错误:OUT_OF_DATE
//!   是控制流不是失败)、[`common::syntax`](错误处理语法糖,frenderer syntax crate
//!   移植,warn + 早退哲学);
//! - [`vulkan`]:Vulkan 资源层——按生命周期分子模块:`context`(进程级:
//!   Entry/Instance/Surface/Device/Queue)、`swapchain`(resize 级:随窗口尺寸重建)、
//!   `frames`(帧级:命令缓冲与同步对象,跨重建轮转复用)、`resources`(资产级:
//!   内存契约,3.2.2.1 定案 memory type/usage/绑定/对齐)、`pool`(资产级:DEVICE_LOCAL
//!   大池 + bump + 驻留缓存,3.2.2.2/3.2.2.3)、`uploader`(批次级:staging 环 + timeline
//!   票据,3.2.4)、`mesh_convert`(纯函数:32B 交错转换,3.2.3)、`images`(资产级:贴图
//!   链路,3.3.1)、`descriptors`(资产级:常驻描述符表 + 槽位发布,3.3.2~3.3.4)、
//!   `pipeline`(接口件:图形管线,3.4.2);
//! - [`scene`]:ECS 侧取数（step3 施工 3.1，3.6.2 分机制/业务两半）——机制半边
//!   `mechanism`(材质缝接线/采集快照/宽高比补位,换场景内容不变)+ 业务半边
//!   `content`(FlightHelmet 进场/取景/灯光参数/施工核验),零 Vulkan 代码;
//! - [`driver`]:渲染驱动(3.6.1 自 src 根收拢)——bevy 调度侧编排,横跨 scene 与 vulkan:
//!   `host`(宿主桥插件:禁渲染补位 + 系统进调度)、`init`(Vulkan 初始化链与反序拆除)、
//!   `frame`(帧循环 draw_frame)、`upload`(上传编排:flush_uploads,Last 里 before
//!   draw_frame);
//! - main.rs:纯组装(装配级配置 + 插件清单;冻结边界只许增删 add_plugins)。

pub mod common;
pub mod driver;
pub mod scene;
pub mod vulkan;
