//! 描述符集布局声明：每个资源族用一份静态 [`SetLayoutDesc`] 写清自己的 binding 表
//!（编号、描述符种类、数量、可见阶段、binding 旗标、布局旗标），统一由
//! [`SetLayoutDesc::create`] 建成 Vulkan 布局。同一份声明还是管线组装 pipeline layout
//! 和 shader ABI 对账的依据（见 `pipeline_layout`）。
//!
//! 布局句柄仍归声明它的资源族持有和销毁；管线通过 [`SetLayouts`] 按声明名取句柄，
//! 不再按位置接收裸句柄。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};

/// 描述符种类（WGSL 资源类型与 Vulkan 描述符类型的公共子集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescKind {
    SampledImage,
    StorageImage,
    Sampler,
    UniformBuffer,
    StorageBuffer,
}

impl DescKind {
    pub(crate) const fn vk(self) -> vk::DescriptorType {
        match self {
            Self::SampledImage => vk::DescriptorType::SAMPLED_IMAGE,
            Self::StorageImage => vk::DescriptorType::STORAGE_IMAGE,
            Self::Sampler => vk::DescriptorType::SAMPLER,
            Self::UniformBuffer => vk::DescriptorType::UNIFORM_BUFFER,
            Self::StorageBuffer => vk::DescriptorType::STORAGE_BUFFER,
        }
    }
}

/// 阶段旗标的 const 合并（ash 的 `|` 不是 const）。
pub(crate) const fn stages(list: &[vk::ShaderStageFlags]) -> vk::ShaderStageFlags {
    let mut raw = 0;
    let mut i = 0;
    while i < list.len() {
        raw |= list[i].as_raw();
        i += 1;
    }
    vk::ShaderStageFlags::from_raw(raw)
}

/// 一个 binding 的声明。
#[derive(Clone, Copy, Debug)]
pub struct BindingDesc {
    pub binding: u32,
    pub kind: DescKind,
    /// 描述符个数；shader 侧非数组时必须是 1。
    pub count: u32,
    pub stages: vk::ShaderStageFlags,
    pub flags: vk::DescriptorBindingFlags,
}

impl BindingDesc {
    /// 单个描述符、无 binding 旗标。
    pub(crate) const fn new(binding: u32, kind: DescKind, stages: vk::ShaderStageFlags) -> Self {
        Self { binding, kind, count: 1, stages, flags: vk::DescriptorBindingFlags::empty() }
    }

    pub(crate) const fn array(self, count: u32, flags: vk::DescriptorBindingFlags) -> Self {
        Self { count, flags, ..self }
    }
}

/// 一个描述符集布局的完整声明。`name` 是它在 [`SetLayouts`] 里的身份，全局唯一。
#[derive(Debug)]
pub struct SetLayoutDesc {
    pub name: &'static str,
    pub bindings: &'static [BindingDesc],
    pub flags: vk::DescriptorSetLayoutCreateFlags,
}

impl SetLayoutDesc {
    pub(crate) const fn new(name: &'static str, bindings: &'static [BindingDesc]) -> Self {
        Self { name, bindings, flags: vk::DescriptorSetLayoutCreateFlags::empty() }
    }

    pub(crate) fn binding(&self, binding: u32) -> Option<&BindingDesc> {
        self.bindings.iter().find(|b| b.binding == binding)
    }

    /// 宿主侧自检：编号不重、数量与阶段非空、UPDATE_AFTER_BIND 三层配套里的布局层，
    /// 以及 UAB 只用在设备启用了的描述符种类上。返回全部问题，空 = 通过。
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.bindings.is_empty() {
            out.push("没有任何 binding".to_owned());
        }
        for (i, b) in self.bindings.iter().enumerate() {
            if self.bindings[..i].iter().any(|prev| prev.binding == b.binding) {
                out.push(format!("b{} 重复声明", b.binding));
            }
            if b.count == 0 {
                out.push(format!("b{} 描述符数量为 0", b.binding));
            }
            if b.stages.is_empty() {
                out.push(format!("b{} 没有可见阶段", b.binding));
            }
            if b.flags.contains(vk::DescriptorBindingFlags::UPDATE_AFTER_BIND) {
                // VUID-VkDescriptorSetLayoutCreateInfo-flags-03000
                if !self.flags.contains(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL) {
                    out.push(format!("b{} 带 UPDATE_AFTER_BIND，布局缺 UPDATE_AFTER_BIND_POOL", b.binding));
                }
                // Context 只启用了 descriptorBindingSampledImageUpdateAfterBind（覆盖 sampled image 与 sampler）
                if !matches!(b.kind, DescKind::SampledImage | DescKind::Sampler) {
                    out.push(format!("b{} 是 {:?}，设备没启用这一类的 UPDATE_AFTER_BIND", b.binding, b.kind));
                }
            }
        }
        out
    }

    /// 布局带 UPDATE_AFTER_BIND binding 时，配套的 pool 必须带 UPDATE_AFTER_BIND 旗标
    ///（VUID-03000 三层配套的第三层，pool 侧由这里推导，不再手写）。
    pub(crate) const fn needs_uab_pool(&self) -> bool {
        self.flags.contains(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
    }

    /// 把 `set_count` 份该布局的集对 pool 的消耗累加进 `out`（按描述符种类聚合）。
    /// 分配即按布局全 binding 扣，与是否写入无关（Vulkan 的 pool 记账口径）。
    pub(crate) fn add_pool_sizes(&self, out: &mut Vec<vk::DescriptorPoolSize>, set_count: u32) {
        for b in self.bindings {
            let total = b.count * set_count;
            if let Some(slot) = out.iter_mut().find(|s| s.ty == b.kind.vk()) {
                slot.descriptor_count += total;
            } else {
                out.push(vk::DescriptorPoolSize::default().ty(b.kind.vk()).descriptor_count(total));
            }
        }
    }

    /// 单份布局 `set_count` 份集的 pool 消耗（单布局 pool 直接用）。
    pub(crate) fn pool_sizes_vec(&self, set_count: u32) -> Vec<vk::DescriptorPoolSize> {
        let mut out = Vec::new();
        self.add_pool_sizes(&mut out, set_count);
        out
    }

    /// binding 表是否为“0..N 连续的单个 storage buffer”——`storage_sets` 的
    /// 前提形状。是则返回 N。
    pub(crate) fn dense_storage(&self) -> Option<u32> {
        for (i, b) in self.bindings.iter().enumerate() {
            if b.binding != i as u32
                || b.count != 1
                || b.kind != DescKind::StorageBuffer
                || !b.flags.is_empty()
            {
                return None;
            }
        }
        u32::try_from(self.bindings.len()).ok().filter(|n| *n > 0)
    }

    /// 自检后建 Vulkan 布局；binding 旗标经 `BindingFlagsCreateInfo` 挂入。
    ///
    /// # Errors
    /// 自检不通过，或布局创建失败。
    pub(crate) fn create(&self, device: &Device) -> Result<vk::DescriptorSetLayout> {
        let problems = self.problems();
        if !problems.is_empty() {
            return Err(VulkanError::Init(format!("描述符集布局 {} 声明有误：{}", self.name, problems.join("；"))));
        }
        let bindings: Vec<vk::DescriptorSetLayoutBinding> = self
            .bindings
            .iter()
            .map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b.binding)
                    .descriptor_type(b.kind.vk())
                    .descriptor_count(b.count)
                    .stage_flags(b.stages)
            })
            .collect();
        let binding_flags: Vec<vk::DescriptorBindingFlags> = self.bindings.iter().map(|b| b.flags).collect();
        let mut flags_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);
        let mut info = vk::DescriptorSetLayoutCreateInfo::default().flags(self.flags).bindings(&bindings);
        if binding_flags.iter().any(|f| !f.is_empty()) {
            info = info.push_next(&mut flags_info);
        }
        Ok(unsafe { device.create_descriptor_set_layout(&info, None) }?)
    }
}

/// 多份布局一起挤一个 pool 时的合并消耗：`(声明, 集数)` 列表逐项累加。
pub(crate) fn merged_pool_sizes(items: &[(&'static SetLayoutDesc, u32)]) -> Vec<vk::DescriptorPoolSize> {
    let mut out = Vec::new();
    for (desc, set_count) in items {
        desc.add_pool_sizes(&mut out, *set_count);
    }
    out
}

/// 一个已建成的布局句柄和它的声明。句柄归资源族，这里只是借出时带上身份。
#[derive(Clone, Copy)]
pub struct SetLayout {
    desc: &'static SetLayoutDesc,
    raw: vk::DescriptorSetLayout,
}

impl SetLayout {
    /// `raw` 必须是由 `desc.create` 建出来的。
    pub(crate) fn new(desc: &'static SetLayoutDesc, raw: vk::DescriptorSetLayout) -> Self {
        Self { desc, raw }
    }

    /// 裸句柄（给不走布局配方的探针用）。
    #[must_use]
    pub fn raw(self) -> vk::DescriptorSetLayout {
        self.raw
    }
}

/// 一个已分配的描述符集，带着它的声明身份。帧录制绑定时据此按管线配方校验
/// 绑定序（debug 断言；release 零成本）。
#[derive(Clone, Copy)]
pub struct DescSet {
    desc: &'static SetLayoutDesc,
    raw: vk::DescriptorSet,
}

impl DescSet {
    /// `raw` 必须按 `desc` 的布局分配。
    pub(crate) fn new(desc: &'static SetLayoutDesc, raw: vk::DescriptorSet) -> Self {
        Self { desc, raw }
    }

    /// 裸句柄（vk 命令消费）。
    #[must_use]
    pub fn raw(self) -> vk::DescriptorSet {
        self.raw
    }

    pub(crate) fn desc(self) -> &'static SetLayoutDesc {
        self.desc
    }
}

/// 绑定序校验：第 i 个集的声明必须是 expected[i]，数量也一并核对。
/// 绑错顺序在 debug 构建当场 panic（声明名写进报错），release 零成本。
#[track_caller]
pub(crate) fn assert_set_sequence(expected: &[&'static SetLayoutDesc], sets: &[DescSet]) {
    debug_assert!(
        expected.len() == sets.len(),
        "绑定序不匹配：管线要 {} 个集，实际给了 {} 个",
        expected.len(),
        sets.len()
    );
    for (i, (want, got)) in expected.iter().zip(sets).enumerate() {
        debug_assert!(
            std::ptr::eq(*want, got.desc()),
            "绑定序错误：set{i} 应为 {}，实际是 {}",
            want.name,
            got.desc().name
        );
    }
}

/// 各资源族借出的布局登记表，管线按声明取句柄。
pub struct SetLayouts {
    entries: Vec<SetLayout>,
}

impl SetLayouts {
    /// # Errors
    /// 同名声明登记了两次，或登记了空句柄。
    pub fn new(layouts: impl IntoIterator<Item = SetLayout>) -> Result<Self> {
        let mut entries: Vec<SetLayout> = Vec::new();
        for layout in layouts {
            if entries.iter().any(|e| e.desc.name == layout.desc.name) {
                return Err(VulkanError::Init(format!("描述符集布局 {} 登记了两次", layout.desc.name)));
            }
            if layout.raw == vk::DescriptorSetLayout::null() {
                return Err(VulkanError::Init(format!("描述符集布局 {} 登记时还没建", layout.desc.name)));
            }
            entries.push(layout);
        }
        Ok(Self { entries })
    }

    /// 按声明取句柄；登记的必须正是这一份声明，不只是同名。
    ///
    /// # Errors
    /// 登记表里没有这份声明。
    pub(crate) fn get(&self, desc: &'static SetLayoutDesc) -> Result<vk::DescriptorSetLayout> {
        self.entries
            .iter()
            .find(|e| std::ptr::eq(e.desc, desc))
            .map(|e| e.raw)
            .ok_or_else(|| VulkanError::Init(format!("描述符集布局 {} 没有登记", desc.name)))
    }
}
