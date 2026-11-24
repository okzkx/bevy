//! 渲染功能，一个功能一个模块：它自己的资源、compute 与图形管线、CPU 侧数学和
//! 录制函数都在里面。只依赖 `core` 和 `resident`；功能之间只共享描述符集布局声明
//!（管线配方要引用），其余都由帧编排传句柄。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | `visibility` | GPU 驱动的可见性链：视锥剔除、Hi-Z、遮挡、LOD 选档、indirect 命令组装 |
//! | `gbuffer` | G-buffer 目标与写入管线 |
//! | `shadow` | 级联阴影：级联矩阵、阴影命令组装、阴影深度图与管线 |
//! | `lighting` | 光源分簇与延迟光照合成 |
//! | `transparent` | 透明层：挑选、排序、命令组装与前向绘制 |
//! | `post` | 后处理：Bloom、tonemap、SMAA |
//! | `sky` | 程序化天空参数（天空函数在 `shaders/common/sky.wgsl`） |
//! | `ibl` | 环境光照：天空 → 环境 cubemap、SH-L2 漫反射、GGX 预滤波高光 |
//! | `rt` | 求交器调试面：RT 描述符集、RGBA16F 调试图、TLAS 组装与主射线两条 compute |
//! | `overlay` | UI：overlay 管线、字体图集、UI 顶点环 |

pub(crate) mod gbuffer;
pub(crate) mod ibl;
pub(crate) mod lighting;
pub(crate) mod overlay;
pub(crate) mod post;
pub(crate) mod rt;
pub(crate) mod shadow;
pub(crate) mod sky;
pub(crate) mod transparent;
pub(crate) mod visibility;
