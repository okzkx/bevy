//! 命令录制助手：绑定与 viewport、图像屏障、compute dispatch 与交接屏障、全屏 pass。
//! 只往命令缓冲里写命令，不创建也不持有任何 Vulkan 对象。

pub(in crate::render) mod barrier;
pub(in crate::render) mod compute;
pub(in crate::render) mod fullscreen;
pub(in crate::render) mod graphics;
