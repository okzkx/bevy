//! gpu 子目录：5.x/6.x 的 compute 半边家族——视锥剔除/Hi-Z 遮挡/LOD 选档/
//! indirect 组装（标记链与命令环的 compute 半边，消费 set2 常驻表）。

pub(crate) mod cull;
pub(crate) mod hiz;
pub(crate) mod indirect;
pub(crate) mod lod;
pub(crate) mod occlusion;
pub(crate) mod shadow_build;
