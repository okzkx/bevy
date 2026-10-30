//! `render` 的最底层，其余目录都建立在它之上。依赖单向，自上而下：
//!
//! | 子目录 | 内容 |
//! |---|---|
//! | `device` | 设备上下文、交换链、内存契约与 buffer、诊断回读、队列传输与 timeline ticket |
//! | `target` | 附件图像：颜色/深度目标与格式常量 |
//! | `binding` | 绑定契约：set 布局声明、shader ABI 表、管线布局配方与对账 |
//! | `pipeline` | 管线骨架：图形句柄、compute、全屏管线的创建与持有 |
//! | `record` | 命令录制助手：绑定、viewport、屏障、dispatch、全屏 pass |
//!
//! 目录间依赖：`target` 只依赖 `device`；`binding` 与 `record` 不依赖 core 里的其他目录；
//! `pipeline` 依赖 `binding`（按配方建）和 `record`（compute 管线交出录制句柄）。
//! `resident` 只用 `device` 与 `binding`。

pub(super) mod binding;
pub(super) mod device;
pub(super) mod pipeline;
pub(super) mod record;
pub(super) mod target;
