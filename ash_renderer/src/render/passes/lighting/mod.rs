//! 光照：光源分簇，以及把 G-buffer、光源分簇和阴影合成成 HDR 光照的合成管线。

pub(crate) mod cluster;
pub(crate) mod composite_pipeline;
