//! 常驻 GPU 数据：跨帧住在显存里、按槽位增量更新的表和池。
//!
//! - [`tables`]：shader 直接读的表（bindless 贴图表、每帧 UBO、实例表、光源表），各 pass 绑上就读。
//! - [`assets`]：bevy 资产转换并驻留进显存（mesh 大池、贴图缓存），由上传链路写入。
//! - [`slot_allocator`]、[`sampler`]：两类共用的槽位账和采样器键。

pub(crate) mod assets;
pub(crate) mod sampler;
pub(crate) mod slot_allocator;
pub(crate) mod tables;
