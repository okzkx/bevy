//! pipeline 子目录：渲染管线主体——帧编排、图形管线、附件对象（8.1 起的
//! deferred/透明层/合成增量都落此目录）。

pub(crate) mod depth;
pub(crate) mod frames;
pub(crate) mod fullscreen_pipeline;
pub(crate) mod gbuffer;
pub(crate) mod graphics_pipeline;
pub(crate) mod overlay_pipeline;
