//! shader 直接读的常驻表：每张表自带描述符 set 或 storage buffer，记录字节布局与 WGSL 对齐。

pub(crate) mod bindless_texture_table;
pub(crate) mod frame_uniform_table;
pub(crate) mod instance_table;
pub(crate) mod light_table;
