//! 工具层（横切）：错误类型、错误处理语法糖与 GPU 记录字节打包，零业务依赖，全库可用。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`bytes`] | 定长 GPU 记录的字节打包（小端标量、列主序矩阵） |
//! | [`error`] | 类型化错误（`VulkanError`：OUT_OF_DATE 是控制流，不是失败）+ 统一错误通道（`tier_handler` 按 Severity 分流，`fatal_gate` 写 AppExit 退出） |
//! | [`syntax`] | 错误处理语法糖（`or_warn`/`or_fatal`/`ok_or_fatal`/`fatal` 一行冒泡、`OnceLog` 一次性日志、`unwrap_or_return!` 判空早退） |
//!
//! 依赖方向：本层是最底层，不依赖任何其他顶层模块；其余各层都可以向下依赖
//! 本层（分层总览见 lib.rs）。

pub mod bytes;
pub mod error;
pub mod syntax;
