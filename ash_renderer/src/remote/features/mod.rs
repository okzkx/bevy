//! BRP 渲染功能读数与开关方法族：一文件对应一个渲染功能，固定套路 =
//! census 对照（GPU 标记 vs CPU 参照）+ set 开关（egui checkbox 同源）。
//! 边界统一：只读标记/只写开关，读数不回灌绘制决策。

pub(crate) mod cluster;
pub(crate) mod cull;
pub(crate) mod draw;
pub(crate) mod gap;
pub(crate) mod lod;
pub(crate) mod render_settings;
pub(crate) mod residency;
pub(crate) mod shadow;
