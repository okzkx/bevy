//! 类型化错误与统一错误通道。
//!
//! 帧循环的核心分支逻辑就是区分 `ERROR_OUT_OF_DATE_KHR`（必须重建后重试，不算错误）
//! 与真失败——`String` 承载不了这个语义，所以引入 enum。初始化失败带调用上下文走
//! `Init(String)`；固定容量耗尽与访问越界走 `Capacity(String)`；运行期编排契约被
//! 打破（状态矛盾、误用）走 `Contract(String)`。
//!
//! 统一错误通道（正本见《代码结构优化：统一错误通道定案》）：system 签名
//! `Result<(), BevyError>`，`?` 冒泡的 Err 由执行器交给 [`tier_handler`]，按错误值的
//! `Severity` 分流——`Warning` 打 warn 后丢弃；其余打 error 并置位 [`FATAL`]，由
//! [`fatal_gate`] 写 `AppExit` 优雅退出。handler 是裸函数指针、无 world 访问权，
//! 退出请求只能经静态位转给 `fatal_gate`。

use std::sync::atomic::{AtomicBool, Ordering};

use ash::vk;
use bevy::app::AppExit;
use bevy::ecs::error::{BevyError, ErrorContext, Severity};
use bevy::ecs::message::MessageWriter;
use bevy::log::{error, info, warn};

use ash_macros::system;

#[derive(Debug, thiserror::Error)]
pub enum VulkanError {
    /// swapchain 与 surface 不再兼容（典型：窗口 resize/最小化）——重建后下一帧重试即可
    #[error("swapchain 已过时（ERROR_OUT_OF_DATE_KHR），需重建后重试")]
    SwapchainOutOfDate,
    #[error("Vulkan 调用失败: {0}")]
    Vk(#[from] vk::Result),
    /// 运行期编排契约被打破：内部状态矛盾或接口误用，状态已不可信
    #[error("运行期编排契约被打破: {0}")]
    Contract(String),
    /// 固定容量耗尽或访问范围越界：显式拒绝，不静默截断、不越界读写
    #[error("容量/范围越界: {0}")]
    Capacity(String),
    #[error("{0}")]
    Init(String),
}

/// 优雅退出请求位：唯一写者 [`tier_handler`]（致命分支置位），消费者
/// [`fatal_gate`] 与帧循环入口（[`fatal_pending`]）。
///
/// 坑：进程级全局静态，多 App 并存会串扰——本项目单进程单 App。
static FATAL: AtomicBool = AtomicBool::new(false);

/// `AppExit` 是否已写过（`fatal_gate` 去重；FATAL 本身不清，帧循环入口据此停手）。
static EXIT_WRITTEN: AtomicBool = AtomicBool::new(false);

/// 是否已有致命错误待退出。
///
/// 帧循环入口（`flush_uploads` / `draw_frame`）先查它：致命失败后同步状态已不可信，
/// 再录一帧可能等一个永远没人 signal 的 fence（提交失败已 reset）而永久阻塞，
/// 连 `fatal_gate` 写的 `AppExit` 都轮不到 runner 观察。
#[must_use]
pub fn fatal_pending() -> bool {
    FATAL.load(Ordering::Acquire)
}

/// 统一错误处理器（`App::set_error_handler` 注册；system/observer/command 的 Err
/// 全走这里）：按 `Severity` 分流。
///
/// - `Warning`：影响不大、下帧自愈，warn 后丢弃，运行继续；
/// - 其余：状态不可信，打 error 并置位 `FATAL`，由 `fatal_gate` 写 `AppExit` 优雅
///   退出。忘标 severity 的错误默认 `Severity::Panic`（官方 `From` 转换语义）且无
///   payload，也走这一支。
///
/// 坑：panic 分支必须保留——真 panic 被 `handle_unwind` 捕获后带着 payload 转进本
/// handler，`resume_unwind` 原样还回去才保得住 backtrace；吞掉 payload 就丢了现场。
pub fn tier_handler(mut err: BevyError, ctx: ErrorContext) {
    if matches!(err.severity(), Severity::Panic)
        && let Some(payload) = err.take_payload()
    {
        std::panic::resume_unwind(payload);
    }
    match err.severity() {
        Severity::Warning => warn!("{ctx}: {err}"),
        // 低档 = 携带方明示可忽略：按 info 记录，不升级为致命退出
        Severity::Ignore | Severity::Trace | Severity::Debug | Severity::Info => {
            info!("{ctx}: {err}");
        }
        _ => {
            error!("{ctx}: {err}");
            FATAL.store(true, Ordering::Release);
        }
    }
}

/// `FATAL` 置位 → 写 `AppExit::error()` 优雅退出（main.rs 挂在 `First`）。
///
/// FATAL 不清：帧循环入口靠 [`fatal_pending`] 停手，退出前不再录帧。`AppExit` 只写
/// 一次（`EXIT_WRITTEN` 去重）。写入后本帧剩余 system 照跑，runner 帧末查到
/// `AppExit` 才停。
#[system]
pub fn fatal_gate(mut exit: MessageWriter<AppExit>) {
    if FATAL.load(Ordering::Acquire) && !EXIT_WRITTEN.swap(true, Ordering::AcqRel) {
        exit.write(AppExit::error());
    }
}

/// crate 级 `Result` 别名：缺省错误 = [`VulkanError`]，第二参可显式改写（如
/// `Result<ImageSpec, ImageSpecError>`）。导入本名会遮蔽 std 同名，需要别的
/// 形态时写全称 `std::result::Result`。
pub type Result<T, E = VulkanError> = std::result::Result<T, E>;
