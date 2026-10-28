//! GPU 驱动的可见性链：每帧在 GPU 上决定画哪些物体、用哪档 LOD，并生成间接绘制命令。

pub(crate) mod cull;
pub(crate) mod hiz;
pub(crate) mod indirect;
pub(crate) mod lod;
pub(crate) mod occlusion;
