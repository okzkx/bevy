//! 级联阴影：CPU 算级联矩阵（`cascades`），GPU 组装每级阴影命令（`shadow_build`），
//! 再画进四张阴影深度图（`shadow_map`）；`shadow_frame` 把两段与恒绑资源收成一帧的录制束。

pub(crate) mod cascades;
pub(crate) mod shadow_build;
pub(crate) mod shadow_frame;
pub(crate) mod shadow_map;
