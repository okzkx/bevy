//! 类型化错误（施工④）。
//!
//! 帧循环的核心分支逻辑就是区分 `ERROR_OUT_OF_DATE_KHR`（必须重建后重试，不算错误）
//! 与真失败——`String` 承载不了这个语义，所以引入 enum。初始化失败仍带调用上下文，
//! 走 `Init(String)`。
//!
//! [`TierError`] 是两 Tier 收尾指令载体（无错误负载——日志在错误点就地记录），
//! 由 [`crate::common::syntax::ResultTierExt`] 产出、system 壳消费。

use ash::vk;
use bevy::log::error;

#[derive(Debug, thiserror::Error)]
pub enum VulkanError {
    /// swapchain 与 surface 不再兼容（典型：窗口 resize/最小化）——重建后下一帧重试即可
    #[error("swapchain 已过时（ERROR_OUT_OF_DATE_KHR），需重建后重试")]
    SwapchainOutOfDate,
    #[error("Vulkan 调用失败: {0}")]
    Vk(#[from] vk::Result),
    #[error("上传/资源编排契约被打破: {0}")]
    Upload(String),
    #[error("{0}")]
    Init(String),
}

/// 系统内部错误的两 Tier 收尾指令：变体即裁决，收尾副作用（让路 / 写 `AppExit`
/// 退出）由调用方的 system 壳统一执行。
///
/// 收尾必须留在 system 壳——Bevy 的 system Result 通道把 Err 交给无 world 访问权
/// 的 error handler，写不了 `AppExit`，两 Tier 的退出裁决进不了 Bevy 通道。
#[derive(Debug, Clone, Copy)]
pub enum TierError {
    /// Tier①：已 warn，丢弃本次（本帧/本批让路，下轮自愈）。
    Warn,
    /// Tier②：已 error，状态已不可信，优雅退出。
    Fatal,
}

impl TierError {
    /// Tier② 就地构造：error 记录后返回 Fatal。match 分支里接裸错误用；
    /// `?` 链上的 Result 用 [`crate::common::syntax::ResultTierExt::or_fatal`]。
    pub fn fatal(msg: &str, e: impl core::fmt::Display) -> Self {
        error!("{msg}: {e}");
        Self::Fatal
    }
}

/// crate 级 `Result` 别名：缺省错误 = [`VulkanError`]，运行期 Vulkan 侧签名共用；
/// 第二参显式可写（如 `Result<ImageSpec, ImageConvertError>`）。导入本名即遮蔽
/// std 同名——文件里需要别的 `Result` 形态时用全称 `std::result::Result`
///（common::syntax 泛型代码的惯例）。
pub type Result<T, E = VulkanError> = std::result::Result<T, E>;
