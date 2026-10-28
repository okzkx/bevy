//! 管线骨架：按布局配方建管线并持有句柄。图形（句柄对）、compute（单入口）、
//! 全屏（无顶点输入的全屏三角形）三种。

pub(in crate::render) mod compute;
pub(in crate::render) mod fullscreen;
pub(in crate::render) mod graphics;
