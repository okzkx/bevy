//! 渲染管线主体，按帧内阶段分组。依赖只朝一个方向：`base` 被各阶段组共用，
//! `frames` 在最上层把各组串成一帧，组与组之间不互相引用内部文件。
//!
//! | 组 | 内容 |
//! |---|---|
//! | `frames` | 帧槽（命令缓冲 + 同步）与整帧录制编排 |
//! | `base` | 管线公共件：图形管线句柄与录制助手、全屏管线骨架、颜色/深度目标、storage 描述符集 |
//! | `geometry` | 画场景几何：G-buffer 目标与写入管线、透明层 |
//! | `lighting` | 光照：光源分簇、级联阴影、延迟光照合成 |
//! | `post` | 后处理：Bloom、tonemap、SMAA |
//! | `overlay` | UI：overlay 管线、字体图集、UI 顶点环 |

pub(crate) mod base;
pub(crate) mod frames;
pub(crate) mod geometry;
pub(crate) mod lighting;
pub(crate) mod overlay;
pub(crate) mod post;
