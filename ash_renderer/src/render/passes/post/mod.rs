//! 后处理：HDR → Bloom → tonemap 到 LDR → SMAA → swapchain。
//!
//! 自动曝光（[`exposure`]）挂在显示段：它读 HDR、写全帧曝光倍数，由 tonemap 乘上，
//! 与 SCGI/IBL 的物理量无关。

pub(crate) mod bloom;
pub(crate) mod color_scope;
pub(crate) mod exposure;
pub(crate) mod post_chain;
pub(crate) mod present_tonemap_pipeline;
pub(crate) mod smaa;
