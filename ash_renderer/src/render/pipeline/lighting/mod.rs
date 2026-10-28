//! 光照：光源分簇、级联阴影，以及把它们和 G-buffer 合成成 HDR 光照的合成管线。

pub(crate) mod cluster;
pub(crate) mod composite_pipeline;
pub(crate) mod shadow;
