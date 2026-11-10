//! 资产驻留：bevy `Mesh`/`Image` 转换后上传进显存，按资产身份去重，负责扩容与延迟回收。

pub(crate) mod bcn;
pub(crate) mod bcn_cache;
pub(crate) mod image_spec;
pub(crate) mod images;
pub(crate) mod mesh_convert;
pub(crate) mod pool;
