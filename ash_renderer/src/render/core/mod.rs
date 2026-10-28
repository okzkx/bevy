//! `render` 的最底层，其余目录都建立在它之上。分两半，依赖单向：
//!
//! | 子目录 | 内容 | 使用方 |
//! |---|---|---|
//! | `device` | 设备上下文、交换链、内存契约与 buffer、诊断回读 | 全部上层（`resident` 只用这一半） |
//! | `kit` | pass 标准件：管线句柄、compute/全屏管线骨架、颜色/深度目标、storage 描述符集、图像屏障 | `passes`、`frame` |
//!
//! `kit` 依赖 `device`，`device` 不知道管线和渲染目标的存在。

pub(super) mod device;
pub(super) mod kit;
