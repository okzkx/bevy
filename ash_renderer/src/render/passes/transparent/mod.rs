//! 透明层：挑出、排序、组装透明物体的绘制命令，再前向画进 HDR 目标。光源读
//! [`crate::render::passes::lighting::cluster`] 的光源分簇集。挑选和排序都在 GPU 上做，CPU 只给在场行数。
//!
//! 每帧顺序：collect 挑出 alpha<1 的行并算深度键 → sort 按深度键排序 →
//! gather 生成间接绘制命令 → 一次 indirect 绘制。
//! 透明绘制把不透明 pass 的深度图重新挂成深度附件，只测不写。坑：挂成附件的
//! 深度图不能同时采样，shader 要读深度就得另拷一份。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `transparent_targets` | 每帧槽的索引列表、深度键、命令环、计数与描述符集 |
//! | `transparent_passes` | collect/sort/gather 三条 compute 管线、帧录制输入与录制函数 |
//! | `transparent_pipeline` | 前向绘制的图形管线 |

mod transparent_passes;
mod transparent_pipeline;
mod transparent_targets;

pub use transparent_passes::{TransparentFrame, TransparentPasses};
pub(crate) use transparent_passes::{
    TRANSPARENT_COLLECT_PIPELINE, TRANSPARENT_GATHER_PIPELINE, TRANSPARENT_SORT_PIPELINE,
};
pub use transparent_pipeline::TransparentPipeline;
pub(crate) use transparent_pipeline::TRANSPARENT_FORWARD_PIPELINE;
pub(crate) use transparent_targets::TRANSPARENT_SET_DESC;
pub use transparent_targets::{TransparentTargets, TRANSPARENT_CAPACITY};
