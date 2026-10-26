//! 类型化错误与统一错误通道。
//!
//! 帧循环的核心分支逻辑就是区分 `ERROR_OUT_OF_DATE_KHR`（必须重建后重试，不算错误）
//! 与真失败——`String` 承载不了这个语义，所以引入 enum。初始化失败带调用上下文走
//! `Init(String)`；固定容量耗尽与访问越界走 `Capacity(String)`；运行期编排契约被
//! 打破（状态矛盾、误用）走 `Upload(String)`。
//!
//! 统一错误通道（正本见《代码结构优化：统一错误通道定案》）：system 签名
//! `Result<(), BevyError>`，`?` 冒泡的 Err 由执行器交给 [`tier_handler`]，按错误值的
//! `Severity` 元数据两 Tier 分流——`Warning` 就地 warn 丢弃（Tier①），其余 error
//! 留档 + 置位 [`FATAL`]，由 [`fatal_gate`] 看门写 `AppExit` 优雅退出（Tier②）。
//! handler 是裸函数指针、无 world 访问权，退出请求只能经静态位中转给看门 system。

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
    #[error("上传/资源编排契约被打破: {0}")]
    Upload(String),
    /// 固定容量耗尽或访问范围越界：显式拒绝，不静默截断、不越界读写
    #[error("容量/范围越界: {0}")]
    Capacity(String),
    #[error("{0}")]
    Init(String),
}

/// 进程级优雅退出请求位：唯一写者 [`tier_handler`]（Tier② 分支置位），唯一消费者
/// [`fatal_gate`]。
///
/// 进程级全局静态，多 App 并存会串扰——本项目单进程单 App，前提钉死。
static FATAL: AtomicBool = AtomicBool::new(false);

/// 统一错误处理器（`App::set_error_handler` 注册；system/observer/command 的 Err
/// 全走这里）：按 `Severity` 元数据两 Tier 分流。
///
/// - `Warning` = Tier①：影响不大、下帧自愈，warn 后丢弃，运行继续；
/// - 其余 = Tier②：状态不可信，error 留档 + 置位 `FATAL`，看门 system 写 `AppExit`
///   优雅退出。忘标 severity 的错误默认 `Severity::Panic`（官方 `From` 转换语义）
///   且无 payload，落到本分支按 Tier② 走——"非必要不 panic"在通道侧坐实。
///
/// panic 岗必须保留：真 panic 被 `handle_unwind` 捕获后带着 payload 转进本 handler，
/// `resume_unwind` 原样还回去，保住 backtrace 取证；吞掉 payload = 丢取证能力。
pub fn tier_handler(mut err: BevyError, ctx: ErrorContext) {
    if matches!(err.severity(), Severity::Panic)
        && let Some(payload) = err.take_payload()
    {
        std::panic::resume_unwind(payload);
    }
    match err.severity() {
        Severity::Warning => warn!("{ctx}: {err}"),
        // 低档（Ignore/Trace/Debug/Info）= 携带方明示可忽略,按 info 记录不升级,
        // 不伪造致命退出
        Severity::Ignore | Severity::Trace | Severity::Debug | Severity::Info => {
            info!("{ctx}: {err}");
        }
        _ => {
            error!("{ctx}: {err}");
            FATAL.store(true, Ordering::Release);
        }
    }
}

/// 看门 system（锚定 `First` 集——多线程执行器下无序系统插入序不保证，早位收益
/// 须显式锚定）：`FATAL` 置位 → 写 `AppExit` 优雅退出。
///
/// swap 清位：连续多个 Tier② 只退一次。置位到生效有半帧延迟——本帧剩余 system
/// 照跑、runner 帧末查 `AppExit` 才停，本帧正常收尾。
#[system]
pub fn fatal_gate(mut exit: MessageWriter<AppExit>) {
    if FATAL.swap(false, Ordering::AcqRel) {
        exit.write(AppExit::error());
    }
}

/// crate 级 `Result` 别名：缺省错误 = [`VulkanError`]，运行期 Vulkan 侧签名共用；
/// 第二参显式可写（如 `Result<ImageSpec, ImageConvertError>`）。导入本名即遮蔽
/// std 同名——文件里需要别的 `Result` 形态时用全称 `std::result::Result`
///（common::syntax 泛型代码的惯例）。
pub type Result<T, E = VulkanError> = std::result::Result<T, E>;
