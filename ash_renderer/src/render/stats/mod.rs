//! 运行时读数与设置：GPU 计时、帧样本账、监控历史、egui pass 计时、帧率同步设置。
//! 供 overlay 监控页与 BRP 读写，不参与渲染本身。

pub(super) mod egui_pass_timing;
pub(super) mod frame_sync_settings;
pub(super) mod monitor_history;
pub(super) mod perf;
