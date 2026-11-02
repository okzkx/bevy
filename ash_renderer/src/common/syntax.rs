//! 错误处理语法糖：统一错误通道的一行糖 + 判空早退宏。
//!
//! 判定线（`.agents/rules/System错误处理规范.md`）：本次失败要求提前 return 就冒泡，
//! 不要求就就地 log 继续。
//! - 冒泡：签名 `Result<(), BevyError>` 的系统，错误点一行"挂 context 负载 + 标
//!   Severity"后 `?` 上交——[`ResultTierExt`]（`or_warn`/`or_fatal`）、
//!   [`OptionTierExt`]（`ok_or_fatal`）、[`fatal`]（match 分支就地构造）。日志由
//!   [`crate::common::error::tier_handler`] 统一打，错误点不 log。
//! - 就地 log 继续：循环内逐项降级（log 后 `continue`）；无失败路径的 `()` 系统判空
//!   早退走 [`unwrap_or_return!`]；一次性日志走 [`OnceLog`]。

use std::borrow::Cow;
use std::convert::Infallible;

use bevy::ecs::error::{BevyError, ContextExt, ResultSeverityExt, Severity};

/// Result 的错误分级：错误点一行完成"挂 context 负载 + 标 Severity"，`?` 把
/// `BevyError` 冒泡进统一错误通道，日志由 [`crate::common::error::tier_handler`]
/// 统一打。用在签名 `Result<(), BevyError>` 的系统里。
pub trait ResultTierExt<T> {
    /// 可自愈失败：挂 `msg`（可 `format!`）+ 标 [`Severity::Warning`]，`?` 后本帧
    /// 让路（资源未就绪等），handler 打 warn。
    fn or_warn(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError>;
    /// 致命失败：挂 `msg`（可 `format!`）+ 标 [`Severity::Error`]，`?` 后 handler
    /// 打 error 并优雅退出（状态已不可信，不得继续）。
    fn or_fatal(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError>;
}

impl<T, E: Into<BevyError>> ResultTierExt<T> for Result<T, E> {
    fn or_warn(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError> {
        self.map_err(|e| e.into())
            .with_context(move || msg)
            .with_severity(Severity::Warning)
    }

    fn or_fatal(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError> {
        self.map_err(|e| e.into())
            .with_context(move || msg)
            .with_severity(Severity::Error)
    }
}

/// Option 判空的收口：一行 `?` 冒泡，代替 let-else + `return Err(...)`/`return Ok(())`。
/// "容量保证后 XX 必在"这类判空矛盾不是断言：按致命错误优雅退出，不 panic。
pub trait OptionTierExt<T> {
    /// None → 挂 `msg`（可 `format!`）标 [`Severity::Error`]，`?` 后优雅退出。
    fn ok_or_fatal(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError>;
    /// None → 挂 `msg`（可 `format!`）标 [`Severity::Warning`]，`?` 后本帧让路
    /// （可自愈早退，handler 打 warn 后丢弃，下帧继续）。
    fn ok_or_warn(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError>;
}

impl<T> OptionTierExt<T> for Option<T> {
    fn ok_or_fatal(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError> {
        self.ok_or_else(|| BevyError::new(Severity::Error, msg.into()))
    }

    fn ok_or_warn(self, msg: impl Into<Cow<'static, str>>) -> Result<T, BevyError> {
        self.ok_or_else(|| BevyError::new(Severity::Warning, msg.into()))
    }
}

/// 裸错误 → 带负载的致命 `BevyError`（match 分支里接出错值用；`?` 链用
/// [`ResultTierExt::or_fatal`]，判空用 [`OptionTierExt::ok_or_fatal`]）。
pub fn fatal(msg: impl Into<Cow<'static, str>>, err: impl Into<BevyError>) -> BevyError {
    let Err(err) = Err::<Infallible, _>(err.into()).context(msg);
    err.with_severity(Severity::Error)
}

/// 就地构造 Warning 级错误（match 分支里作废本帧用；`?` 链用
/// [`ResultTierExt::or_warn`]，判空用 [`OptionTierExt::ok_or_warn`]）。
pub fn warning(msg: impl Into<Cow<'static, str>>) -> BevyError {
    BevyError::new(Severity::Warning, msg.into())
}

/// 一次性日志：首次 [`OnceLog::log`] 执行 `f` 并置位，此后静默。代替"未置位则置位
/// 并打日志"三行样板。
#[derive(Default)]
pub struct OnceLog(bool);

impl OnceLog {
    pub fn log(&mut self, f: impl FnOnce()) {
        if !self.0 {
            self.0 = true;
            f();
        }
    }
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

pub use unwrap_or_return;
