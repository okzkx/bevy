//! 画场景几何：不透明物体写 G-buffer，透明物体前向画进 HDR 目标。

pub(crate) mod gbuffer;
pub(crate) mod gbuffer_pipeline;
pub(crate) mod transparent;
