//! 工具层（横切）：错误类型与错误处理语法糖，零业务依赖，全库可用。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`error`] | 类型化错误（`VulkanError`：OUT_OF_DATE 是控制流，不是失败；`TierError`：两 Tier 收尾指令） |
//! | [`syntax`] | 错误处理语法糖（frenderers 移植宏族 + `ResultTierExt` 两 Tier 分流；warn + 早退哲学） |
//!
//! 依赖方向：本层不依赖 scene/driver/vulkan 任何一方；vulkan → common 是唯一的
//! 向上依赖（3.6.1 分层定案，见 lib.rs 总览）。

pub mod error;
pub mod syntax;
