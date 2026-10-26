//! pipeline 子目录：渲染管线主体——帧编排、图形管线、附件对象（deferred/透明层/
//! 合成/后处理都落此目录）。

use crate::render::Context;

pub(crate) mod atlas_gpu;
pub(crate) mod depth;
pub(crate) mod frames;
pub(crate) mod cluster;
pub(crate) mod fullscreen_pipeline;
pub(crate) mod gbuffer;
pub(crate) mod graphics_pipeline;
pub(crate) mod overlay_pipeline;
pub(crate) mod pipeline_handle;
pub(crate) mod post;
pub(crate) mod shadow;
pub(crate) mod smaa;
pub(crate) mod ui_vertex_ring;

/// 资源 CONCURRENT 共享的队列族列表：transfer 族与图形族不同时为两族，同族时
/// 为空（= EXCLUSIVE）。
fn concurrent_families(ctx: &Context) -> Vec<u32> {
    if ctx.transfer_queue_family_index != ctx.queue_family_index {
        vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
    } else {
        Vec::new()
    }
}
