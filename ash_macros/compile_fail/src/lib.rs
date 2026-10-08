//! 占位目标：让 `cargo build` 有东西可编。
//!
//! 本 crate 只有 compile-fail 测试；没有构建目标时，`cargo` 不会编译
//! proc-macro 依赖，`ui_test` 的 `DependencyBuilder` 就拿不到 `ash_macros` 的
//! 产物（no artifact found）。lib 重出口宏即补上这个构建目标。
pub use ash_macros::system;
