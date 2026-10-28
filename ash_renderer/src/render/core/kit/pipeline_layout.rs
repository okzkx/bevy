//! 管线的布局配方：用哪个 shader 的哪些入口、按 set 编号排好的布局声明、push 范围。
//! 建 pipeline layout 前先拿配方和 shader ABI 表对账，对不上就拒绝创建并列出每一处问题；
//! 通过后从 [`SetLayouts`] 按声明取句柄组装。
//!
//! 对账口径：shader 入口用到的每个 `(set, binding)` 必须在第 set 个声明里有同种类的
//! binding，数量覆盖 shader 数组长度，可见阶段含该入口的阶段；runtime 长度数组
//!（bindless 表）还要求 PARTIALLY_BOUND——宿主按槽位逐个发布，从不写满。布局里
//! shader 没用到的 binding 不算错。入口用了 push 时，push 范围要覆盖它的字节数和阶段。

use std::ffi::CStr;

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};

use super::compute_pipeline::create_shader_module;
use super::set_layout::{SetLayoutDesc, SetLayouts};
use super::shader_abi::{ArrayShape, Shader};

/// push 范围（偏移恒 0）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PushRange {
    pub stages: vk::ShaderStageFlags,
    pub size: u32,
}

/// 一条管线的布局配方。`sets[i]` 就是 set i。
#[derive(Debug)]
pub(crate) struct PipelineDesc {
    pub label: &'static str,
    pub shader: &'static Shader,
    pub entries: &'static [&'static CStr],
    pub sets: &'static [&'static SetLayoutDesc],
    pub push: Option<PushRange>,
}

impl PipelineDesc {
    /// 配方与 shader ABI 的全部不符项，空 = 通过（口径见模块注释）。
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for desc in self.sets {
            out.extend(desc.problems().into_iter().map(|p| format!("{}: {p}", desc.name)));
        }
        for name in self.entries {
            let name = name.to_str().unwrap_or("<非 UTF-8>");
            let Some(entry) = self.shader.entry(name) else {
                out.push(format!("{} 里没有入口 {name}", self.shader.name));
                continue;
            };
            for r in entry.resources {
                let at = format!("{name} @group({}) @binding({})", r.set, r.binding);
                let Some(set) = self.sets.get(r.set as usize) else {
                    out.push(format!("{at}：管线只声明了 {} 个 set", self.sets.len()));
                    continue;
                };
                let Some(b) = set.binding(r.binding) else {
                    out.push(format!("{at}：{} 里没有这个 binding", set.name));
                    continue;
                };
                if b.kind != r.kind {
                    out.push(format!("{at}：shader 是 {:?}，{} 声明的是 {:?}", r.kind, set.name, b.kind));
                }
                match r.array {
                    ArrayShape::Single if b.count != 1 => {
                        out.push(format!("{at}：shader 不是数组，{} 声明了 {} 个", set.name, b.count));
                    }
                    ArrayShape::Runtime if !b.flags.contains(vk::DescriptorBindingFlags::PARTIALLY_BOUND) => {
                        out.push(format!("{at}：shader 是 runtime 数组，{} 缺 PARTIALLY_BOUND", set.name));
                    }
                    _ => {}
                }
                if !b.stages.contains(entry.stage) {
                    out.push(format!("{at}：{} 的可见阶段 {:?} 不含 {:?}", set.name, b.stages, entry.stage));
                }
            }
            if entry.immediate_size > 0 {
                match self.push {
                    None => out.push(format!("{name} 用了 {}B push，管线没声明 push 范围", entry.immediate_size)),
                    Some(push) => {
                        if push.size < entry.immediate_size {
                            out.push(format!("{name} 用了 {}B push，push 范围只有 {}B", entry.immediate_size, push.size));
                        }
                        if !push.stages.contains(entry.stage) {
                            out.push(format!("{name} 用了 push，push 范围阶段 {:?} 不含 {:?}", push.stages, entry.stage));
                        }
                    }
                }
            }
        }
        out
    }

    /// 对账，不过即报错。
    ///
    /// # Errors
    /// 任一不符项。
    pub(crate) fn check(&self) -> Result<()> {
        let problems = self.problems();
        if problems.is_empty() {
            return Ok(());
        }
        Err(VulkanError::Init(format!(
            "管线 {} 的布局配方与 shader {} 对不上：\n  - {}",
            self.label,
            self.shader.name,
            problems.join("\n  - ")
        )))
    }

    /// 对账后组装 pipeline layout。
    ///
    /// # Errors
    /// 对账不过、有声明没登记，或创建失败。
    pub(crate) fn create_layout(&self, device: &Device, layouts: &SetLayouts) -> Result<vk::PipelineLayout> {
        self.check()?;
        let set_layouts = self.sets.iter().map(|d| layouts.get(d)).collect::<Result<Vec<_>>>()?;
        let push_ranges: Vec<vk::PushConstantRange> = self
            .push
            .iter()
            .map(|p| vk::PushConstantRange::default().stage_flags(p.stages).offset(0).size(p.size))
            .collect();
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_ranges);
        Ok(unsafe { device.create_pipeline_layout(&info, None) }?)
    }

    /// 建管线的公共骨架：对账并组装 layout → shader module → 各入口的阶段信息（阶段
    /// 取自 ABI 表）→ 交给 `assemble` 建管线本体。module 在 `assemble` 返回后销毁；
    /// `assemble` 成功即接管 layout，失败则 layout 在这里销毁。
    ///
    /// # Errors
    /// 对账不过、layout / module 创建失败，或 `assemble` 的错误。
    pub(crate) fn build<T>(
        &self,
        device: &Device,
        layouts: &SetLayouts,
        assemble: impl FnOnce(vk::PipelineLayout, &[vk::PipelineShaderStageCreateInfo<'static>]) -> Result<T>,
    ) -> Result<T> {
        let layout = self.create_layout(device, layouts)?;
        let result = create_shader_module(device, self.shader.spv).and_then(|module| {
            let stages: Vec<_> = self
                .entries
                .iter()
                .filter_map(|name| {
                    let entry = self.shader.entry(name.to_str().ok()?)?;
                    Some(vk::PipelineShaderStageCreateInfo::default().stage(entry.stage).module(module).name(name))
                })
                .collect();
            let result = assemble(layout, &stages);
            unsafe { device.destroy_shader_module(module, None) };
            result
        });
        if result.is_err() {
            unsafe { device.destroy_pipeline_layout(layout, None) };
        }
        result
    }
}
