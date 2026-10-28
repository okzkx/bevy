//! 各 pass 共用的标准件：建管线（图形管线句柄、compute 管线、全屏管线骨架）、
//! 颜色/深度目标、storage 描述符集、图像屏障；以及布局声明（set 布局、shader ABI 表、
//! 管线布局配方与对账）。

pub(in crate::render) mod barrier;
pub(in crate::render) mod color_target;
pub(in crate::render) mod compute_pipeline;
pub(in crate::render) mod depth_target;
pub(in crate::render) mod fullscreen_pipeline;
pub(in crate::render) mod pipeline_handle;
pub(in crate::render) mod pipeline_layout;
pub(in crate::render) mod set_layout;
pub(in crate::render) mod shader_abi;
pub(in crate::render) mod storage_sets;

#[cfg(test)]
mod layout_contract_tests;
