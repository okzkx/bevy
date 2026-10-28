//! 运行时读数：GPU 计时、帧样本账、监控历史、egui pass 计时。供 overlay 监控页与
//! BRP 读取，不参与渲染本身。

pub(super) mod egui_pass_timing;
pub(super) mod monitor_history;
pub(super) mod perf;
