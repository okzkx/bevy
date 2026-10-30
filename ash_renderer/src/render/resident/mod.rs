//! 常驻 GPU 数据：跨帧住在显存里、按槽位增量更新的表和池——mesh 大池、贴图缓存、
//! bindless 描述符表、每帧 UBO、实例表、光源表。各 pass 只读这些表。

pub(crate) mod bindless_texture_table;
pub(crate) mod frame_uniform_table;
pub(crate) mod images;
pub(crate) mod instance_table;
pub(crate) mod light_table;
pub(crate) mod mesh_convert;
pub(crate) mod pool;
pub(crate) mod slot_allocator;
