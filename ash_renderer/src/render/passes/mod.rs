//! 渲染功能，一个功能一个模块：它自己的资源、compute 与图形管线、CPU 侧数学和
//! 录制函数都在里面。只依赖 `core` 和 `resident`，功能之间通过帧编排传句柄，
//! 不互相引用内部文件。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | `visibility` | GPU 驱动的可见性链：视锥剔除、Hi-Z、遮挡、LOD 选档、indirect 命令组装 |
//! | `gbuffer` | G-buffer 目标与写入管线 |
//! | `shadow` | 级联阴影：级联矩阵、阴影命令组装、阴影深度图与管线 |
//! | `lighting` | 光源分簇与延迟光照合成 |
//! | `transparent` | 透明层：挑选、排序、命令组装与前向绘制 |
//! | `post` | 后处理：Bloom、tonemap、SMAA |
//! | `overlay` | UI：overlay 管线、字体图集、UI 顶点环 |

pub(crate) mod gbuffer;
pub(crate) mod lighting;
pub(crate) mod overlay;
pub(crate) mod post;
pub(crate) mod shadow;
pub(crate) mod transparent;
pub(crate) mod visibility;
