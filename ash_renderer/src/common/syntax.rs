//! 错误处理语法糖：统一错误通道的一行糖 + 控制流宏族。
//!
//! 判定线（`.agents/rules/System错误处理规范.md`）：本次失败要求提前 return 就冒泡，
//! 不要求就就地 log 继续。
//! - 冒泡：签名 `Result<(), BevyError>` 的系统，错误点一行"挂 context 负载 + 标
//!   Severity"后 `?` 上交——[`ResultTierExt`]（`or_warn`/`or_fatal`）、
//!   [`OptionTierExt`]（`ok_or_fatal`）、[`fatal`]（match 分支就地构造）。日志由
//!   [`crate::common::error::tier_handler`] 统一打，错误点不 log。
//! - 就地 log 继续：循环内逐项降级（log 后 `continue`）与无失败路径的 `()` 系统，
//!   走控制流宏族；一次性收账日志走 [`OnceLog`]。
//!
//! 宏命名规律：**前缀定输入，后缀定出口**。
//! 前缀：`unwrap_`=Option、`warn_unwrap_`=Result（先打 warn 日志）、`or_`=bool、
//! `matches_`=任意模式 let-else。
//! 后缀：`_return` 结尾 = `return 值`（单参版返回 `Default::default()`）；
//! 无后缀 = 执行给定的发散语句（循环里几乎全是 `continue`）。
//! 用法：`use crate::common::syntax::宏名;`——宏内部用 `$crate::` 全限定调 trait
//! 方法，调用方无需导 trait。

use std::convert::Infallible;

use bevy::ecs::error::{BevyError, ContextExt, ResultSeverityExt, Severity};
use bevy::log::warn;

/// Result 旁路日志：错误打 warn 日志后**原样穿透**（交给 `warn_unwrap_*` 宏早退）。
pub trait LogDebug<T> {
    /// 打完整 Debug，warn 级
    fn warn(self) -> Self;
}

impl<T, D: std::fmt::Debug> LogDebug<T> for std::result::Result<T, D> {
    fn warn(self) -> Self {
        self.inspect_err(|e| warn!("{e:?}"))
    }
}

/// Result 的两 Tier 分流：错误点一行完成"挂 context 负载 + 标 Severity"，`?` 把
/// `BevyError` 冒泡给执行器进统一错误通道——日志在 [`crate::common::error::tier_handler`]
/// 统一打（单点零重复），退出收尾在看门 system。
///
/// 与 [`LogDebug`] 的分工：`warn()` 是旁路日志（Err 原样穿透、不进通道），本
/// trait 产出的是要冒泡的错误。主场 = 签名 `Result<(), BevyError>` 的
/// 系统（现役：`draw_frame`/`flush_uploads`/`init_vulkan`）。
pub trait ResultTierExt<T, E>: Sized {
    /// Tier①：挂 `msg` context（可 `format!`）+ 标 [`Severity::Warning`]——`?` 后
    /// 本帧让路（资源未就绪/可自愈失败），handler warn 统一打日志。
    fn or_warn(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError>;
    /// Tier②：挂 `msg` context（可 `format!`）+ 标 [`Severity::Error`]——`?` 后进
    /// 统一通道，error 留档 + 置位看门优雅退出（状态已不可信，不得继续）。
    fn or_fatal(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError>;
}

impl<T, E: Into<BevyError>> ResultTierExt<T, E> for std::result::Result<T, E> {
    fn or_warn(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError> {
        self.map_err(|e| e.into())
            .with_context(move || msg)
            .with_severity(Severity::Warning)
    }

    fn or_fatal(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError> {
        self.map_err(|e| e.into())
            .with_context(move || msg)
            .with_severity(Severity::Error)
    }
}

/// Option 的 Tier② 收口：判空矛盾/判空缺席一行 `?` 冒泡——**判空矛盾不是断言岗**
/// （"容量保证后 XX 必在"这类状态矛盾按 Tier② 优雅退出，不 panic），let-else +
/// `return Err(...)` 样板禁用，一律收成本 trait 的一行。
pub trait OptionTierExt<T>: Sized {
    /// None → Tier②：挂 `msg` 负载（可 `format!`）标 [`Severity::Error`]，`?` 后
    /// 进统一通道（error 留档 + 置位看门优雅退出）。
    fn ok_or_fatal(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError>;
}

impl<T> OptionTierExt<T> for Option<T> {
    fn ok_or_fatal(self, msg: impl Into<std::borrow::Cow<'static, str>>) -> std::result::Result<T, BevyError> {
        self.ok_or_else(|| BevyError::new(Severity::Error, msg.into()))
    }
}

/// Tier② 就地构造：裸错误 → 带负载的 `BevyError`（match 分支里接出错值用；
/// `?` 链上的 Result 用 [`ResultTierExt::or_fatal`]，Option 判空用
/// [`OptionTierExt::ok_or_fatal`]）。context 负载 = 出错含义（可 format!），
/// 日志由统一 handler 打。
pub fn fatal(
    msg: impl Into<std::borrow::Cow<'static, str>>,
    err: impl Into<BevyError>,
) -> BevyError {
    let Err(err) = Err::<Infallible, _>(err.into()).context(msg);
    err.with_severity(Severity::Error)
}

/// 一次性收账日志标准件：首次 [`OnceLog::log`] 执行并置位，此后静默。
/// 收账旗标 + "未置位则置位并打日志"三行样板的标准下沉——核心代码处一行调用。
#[derive(Default)]
pub struct OnceLog(bool);

impl OnceLog {
    /// 首次调用执行 `f`（打日志/收账），之后静默。
    pub fn log(&mut self, f: impl FnOnce()) {
        if !self.0 {
            self.0 = true;
            f();
        }
    }
}

/// Result 失败 → warn 日志 → `return`（缺省返回 `Default::default()`）。
#[macro_export]
macro_rules! warn_unwrap_or_return {
    ($res_value:expr, $return_result:expr) => {{
        let Ok(t) = $crate::common::syntax::LogDebug::warn($res_value) else {
            return $return_result;
        };
        t
    }};

    ($res_value:expr) => {{
        let Ok(t) = $crate::common::syntax::LogDebug::warn($res_value) else {
            return Default::default();
        };
        t
    }};
}

/// Result 失败 → warn 日志 → 执行发散语句（let-else 保证其必须发散，如 `continue`）。
#[macro_export]
macro_rules! warn_unwrap_or {
    ($res_value:expr, $return_result:expr) => {{
        let Ok(t) = $crate::common::syntax::LogDebug::warn($res_value) else {
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

/// 布尔卫语句：false → `return Default::default()`。
/// 坑：与 let-else 族不同，`or!` 的 `$e2` 不经编译器强制发散，传非发散表达式会
/// 静默继续——只传 `return`/`continue`。
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
    matches_or, matches_or_return, or, or_return, unwrap_or, unwrap_or_return,
    warn_unwrap_or, warn_unwrap_or_return,
};
