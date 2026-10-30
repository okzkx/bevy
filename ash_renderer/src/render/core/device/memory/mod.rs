//! 内存契约：buffer 用途角色的规格、设备内存事实与类型选择、按契约创建的 buffer。
//! 内存类型、usage、绑定与对齐的规则只在这里定，池、缓存和上传器只引用。

pub(in crate::render) mod buffer;
pub(in crate::render) mod contract;
pub(in crate::render) mod role;
