//! 绑定契约：描述符集布局声明、shader ABI 表、管线布局配方。声明 → 与 shader 对账 →
//! 创建 Vulkan 布局与 shader module 都在这里；另有只含 storage buffer 的描述符集分配。

pub(in crate::render) mod pipeline_layout;
pub(in crate::render) mod descriptor_writer;
pub(in crate::render) mod set_layout;
pub(in crate::render) mod shader_abi;
pub(in crate::render) mod storage_sets;

#[cfg(test)]
mod layout_contract_tests;
