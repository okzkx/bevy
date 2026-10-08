//! 渲染驱动层：bevy 调度侧的编排插件，横跨 [`crate::scene`]（读 `CollectedScene`
//! 快照）与 [`crate::vulkan`]（驱动资源）——ECS 世界 ↔ GPU 资源的翻译层。
//!
//! 命名澄清：这里的"驱动"指驱动帧循环与上传链的编排代码，**不是 GPU 驱动**。
//!
//! | 子模块 | 职责 | 调度位置 |
//! |---|---|---|
//! | [`host`] | 宿主桥:插件 + 禁渲染补位 + 系统进调度(系统本体在其子模块 `init`/`frame`) | 插件装配 |
//! | [`upload`] | 上传编排（flush_uploads）：快照去重 → 转换 → 容量保证 → 合批提交 | Last（before draw_frame） |
//!
//! 层内依赖：upload 的排序引用 [`host::draw_frame`]（"先上传后画"），host 不引用
//! upload——依赖单向；全部模块只消费 [`crate::vulkan`] 重出口的类型。

pub mod host;
pub mod upload;

pub use host::AshHostPlugin;
pub use upload::AshUploadPlugin;
