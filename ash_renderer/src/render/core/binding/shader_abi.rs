//! shader ABI 表：build.rs 用 naga 逐入口点反射出的资源使用（`OUT_DIR/shader_abi.rs`），
//! 每个 shader 一个 [`Shader`] 静态项，名字是文件名大写（`gbuffer_write.wgsl` →
//! `GBUFFER_WRITE`），spv 字节也挂在上面，管线取 shader 只认这一处。
//!
//! 表里只有入口真正用到的全局量：声明了却没被入口读写的 binding 不出现。

use ash::{vk, Device};

use super::set_layout::DescKind;
use crate::common::error::{Result, VulkanError};

/// shader 侧的数组形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArrayShape {
    /// 非数组。
    Single,
    /// `binding_array<T>`（长度在运行期定）。定长 binding_array 没有接入，build.rs 遇到即报错。
    Runtime,
}

/// 入口用到的一个资源。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResourceUse {
    pub set: u32,
    pub binding: u32,
    pub kind: DescKind,
    pub array: ArrayShape,
}

/// 一个入口点：名字、阶段、push（var<immediate>）字节数（0 = 不用 push）、用到的资源。
#[derive(Debug)]
pub(crate) struct EntryAbi {
    pub name: &'static str,
    pub stage: vk::ShaderStageFlags,
    pub immediate_size: u32,
    pub resources: &'static [ResourceUse],
}

/// 一个编译好的 shader 模块。
#[derive(Debug)]
pub(crate) struct Shader {
    pub name: &'static str,
    /// SPIR-V 小端字节（不保证 4 字节对齐）。
    pub spv: &'static [u8],
    pub entries: &'static [EntryAbi],
}

impl Shader {
    pub(crate) fn entry(&self, name: &str) -> Option<&EntryAbi> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// 建 shader module（调用方负责销毁）。字节按小端 4B 一字重组：`include_bytes!`
    /// 不保证 u32 对齐，不能直接转型。
    ///
    /// # Errors
    /// `vkCreateShaderModule` 失败。
    pub(crate) fn create_module(&self, device: &Device) -> Result<vk::ShaderModule> {
        let words: Vec<u32> = self.spv.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None) }
            .map_err(VulkanError::from)
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "生成表覆盖全部 shader；`ALL` 和只给 examples 用的条目只有测试引用"))]
mod generated {
    use super::{vk, ArrayShape, DescKind, EntryAbi, ResourceUse, Shader};

    include!(concat!(env!("OUT_DIR"), "/shader_abi.rs"));
}

pub(crate) use generated::*;
