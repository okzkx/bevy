//! BRP 全帧对账与监控方法族：环与常驻表内容对账（[`ring_census`]）、性能读数
//! 与 workload 开关（[`perf`]）、持续监控读数与帧率同步（[`monitor`]）、布局
//! 契约清单（[`layout_contract`]）。帧级/全局证据通道，不隶属单一渲染功能。

pub(crate) mod layout_contract;
pub(crate) mod monitor;
pub(crate) mod perf;
pub(crate) mod ring_census;
