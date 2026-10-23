//! gpu 子目录：compute 半边家族——视锥剔除/Hi-Z 遮挡/LOD 选档/indirect 组装/
//! shadow 命令组装（标记链与命令环的 compute 半边，消费 set2 常驻表）+ 共用的
//! compute 管线标准件（`compute`）。

pub(crate) mod compute;
pub(crate) mod cull;
pub(crate) mod hiz;
pub(crate) mod indirect;
pub(crate) mod lod;
pub(crate) mod occlusion;
pub(crate) mod shadow_build;
