//! 语法糖（移植自用户在 frenderer 的 `modules/common/syntax` crate，错误处理哲学的载体）。
//!
//! frenderer 实践的设计哲学：**错误就地消化——日志 + 安全降级，不上浮、不 panic**。
//! 全库 anyhow 单类型 + 零裸 unwrap；本 crate 借用其控制流族，`log` 换 `bevy::log`。
//!
//! 搬入清单与取舍：
//! - ✅ 控制流宏 9 件（8 件 frenderer 原件 + `unwrap_or_panic!` 为 ash_renderer 新增，
//!   补初始化路径的家族位）+ `LogDebug`/`WarnOrDefault`——bevy 系统返回 `()`，`?` 不可用，
//!   "warn + 早退"正是 `()` 系统里的传播形式；step3 起逐实体收集（Query → 绘制列表）
//!   就是 `unwrap_or!(x, continue)` 的主场（frenderer render_params.rs:35 同形状）；
//! - ❌ `singleton` 系列：bevy `Resource` 就是全局状态的正解，搬进来反而诱导反模式；
//! - ❌ `lock_mutex!`：bevy 调度器管并发，系统内不持手动锁；首个后台线程出现时再议；
//! - ❌ `Option::some()`：anyhow `?` 链专用，这里 Option 早退已由 `unwrap_or_*` 覆盖。
//!
//! 宏命名规律：**前缀定输入，后缀定出口**。
//! 前缀：`unwrap_`=Option、`warn_unwrap_`=Result（先打 warn 日志）、`or_`=bool、
//! `matches_`=任意模式 let-else。
//! 后缀：`_return` 结尾 = `return 值`（单参版返回 `Default::default()`）；
//! 无后缀 = 执行给定的发散语句（循环里几乎全是 `continue`）；
//! `_panic` = 家族的 panic 位（**工程原则"非必要不 panic"——现无现役调用点**，
//! 仅供真正必要的断言场景：不可恢复的内部不变量且需要 backtrace 取证；经
//! `UnwrapPanic` trait 同时吃 Option 与 Result）。
//! 用法：`use ash_renderer::syntax::宏名;`——宏内部用 `$crate::` 全限定调 trait 方法，
//! 调用方无需导 trait。多数宏为 step3+ 预备，本模块关闭 `unused_macros`。

#![allow(unused_macros)]

use bevy::log::{info, warn};

/// Result 旁路日志：错误打日志后**原样穿透**（可继续 `?` 传播或交给宏早退）。
pub trait LogDebug<T> {
    /// 只打 Debug 首行，info 级
    fn info(self) -> Self;
    /// 打完整 Debug，warn 级
    fn warn(self) -> Self;
    /// warn 后降级为 Option
    fn warn_ok(self) -> Option<T>;
}

impl<T, D: std::fmt::Debug> LogDebug<T> for std::result::Result<T, D> {
    fn info(self) -> Self {
        self.map_err(|d| {
            format!("{d:?}").lines().next().inspect(|l| info!("{l}"));
            d
        })
    }

    fn warn(self) -> Self {
        self.map_err(|e| {
            warn!("{e:?}");
            e
        })
    }

    fn warn_ok(self) -> Option<T> {
        self.warn().ok()
    }
}

/// warn 后以 `Default` 兜底：线程入口/后台任务的"失败不炸"语义。
pub trait WarnOrDefault<T> {
    fn warn_unwrap_or_default(self) -> T;
}

impl<T: Default, D: std::fmt::Debug> WarnOrDefault<T> for std::result::Result<T, D> {
    fn warn_unwrap_or_default(self) -> T {
        self.or_else(|e| {
            warn!("{e:?}");
            Ok::<T, D>(Default::default())
        })
        .unwrap_or_default()
    }
}

/// Option 与 Result 的统一解包底座（`unwrap_or_panic!` 用）：错误统一成 `String`（Display）。
/// 两个 blanket impl 实现"Option 与 Result 同一套语法"——frenderer 哲学的延伸件。
pub trait UnwrapPanic<T> {
    fn into_result(self) -> std::result::Result<T, String>;
}

impl<T, E: std::fmt::Display> UnwrapPanic<T> for std::result::Result<T, E> {
    fn into_result(self) -> std::result::Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

impl<T> UnwrapPanic<T> for Option<T> {
    fn into_result(self) -> std::result::Result<T, String> {
        self.ok_or_else(|| "None".to_string())
    }
}

/// Result 失败 → warn 日志 → `return`（缺省返回 `Default::default()`）。
#[macro_export]
macro_rules! warn_unwrap_or_return {
    ($res_value:expr, $return_result:expr) => {{
        let Ok(t) = $crate::syntax::LogDebug::warn($res_value) else {
            return $return_result;
        };
        t
    }};

    ($res_value:expr) => {{
        let Ok(t) = $crate::syntax::LogDebug::warn($res_value) else {
            return Default::default();
        };
        t
    }};
}

/// Result 失败 → warn 日志 → 执行发散语句（let-else 保证其必须发散，如 `continue`）。
#[macro_export]
macro_rules! warn_unwrap_or {
    ($res_value:expr, $return_result:expr) => {{
        let Ok(t) = $crate::syntax::LogDebug::warn($res_value) else {
            $return_result;
        };
        t
    }};
}

/// Option 为 None → `return`（缺省返回 `Default::default()`）。
#[macro_export]
macro_rules! unwrap_or_return {
    ($res_value:expr, $return_result:expr) => {{
        let Some(t) = $res_value else {
            return $return_result;
        };
        t
    }};

    ($res_value:expr) => {{
        let Some(t) = $res_value else {
            return Default::default();
        };
        t
    }};
}

/// Option 为 None → 执行发散语句（let-else 保证其必须发散，如 `continue`）。
#[macro_export]
macro_rules! unwrap_or {
    ($res_value:expr, $return_result:expr) => {{
        let Some(t) = $res_value else {
            $return_result;
        };
        t
    }};
}

/// 失败 → panic（初始化路径专用，ash_renderer 新增件；frenderel 无此宏）。
/// 同时吃 Result（错误走 Display）与 Option（报 "None"）——一次性装配代码
/// 没有"安全降级"可言，fail fast：根因必须钉在启动现场，而不是延后失真。
/// 双参版 `panic!("{上下文}: {错误}")`，单参版直接 panic 错误。
#[macro_export]
macro_rules! unwrap_or_panic {
    ($res_value:expr, $panic_message:expr) => {
        match $crate::syntax::UnwrapPanic::into_result($res_value) {
            Ok(t) => t,
            Err(msg) => panic!("{}: {msg}", $panic_message),
        }
    };

    ($res_value:expr) => {
        match $crate::syntax::UnwrapPanic::into_result($res_value) {
            Ok(t) => t,
            Err(msg) => panic!("{msg}"),
        }
    };
}

/// 布尔卫语句：false → `return Default::default()`。
/// 注意与 let-else 族不同，这里 `$e2` 不经编译器强制发散（沿用 frenderer 原样），
/// 传非发散表达式会静默继续——只传 `return`/`continue`。
#[macro_export]
macro_rules! or_return {
    ($e:expr) => {{
        if !$e {
            return Default::default();
        };
    }};
}

/// 布尔卫语句：false → 执行发散语句（同上，发散性不经编译器强制）。
#[macro_export]
macro_rules! or {
    ($e:expr, $e2:expr) => {{
        if !$e {
            $e2;
        };
    }};
}

/// 任意模式 let-else：不匹配 → `return`（缺省返回 `Default::default()`）。
#[macro_export]
macro_rules! matches_or_return {
    ($pattern:pat, $expression:expr) => {
        let $pattern = $expression else {
            return Default::default();
        };
    };
    ($pattern:pat, $expression:expr, $return_result:expr) => {
        let $pattern = $expression else {
            return $return_result;
        };
    };
}

/// 任意模式 let-else：不匹配 → 执行发散语句。
#[macro_export]
macro_rules! matches_or {
    ($pattern:pat, $expression:expr, $return_result:expr) => {
        let $pattern = $expression else {
            $return_result;
        };
    };
}

pub use {
    matches_or, matches_or_return, or, or_return, unwrap_or, unwrap_or_panic,
    unwrap_or_return, warn_unwrap_or, warn_unwrap_or_return,
};
