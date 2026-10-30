//! 设备内存事实与选型：启动时查一次内存类型/堆表和 `nonCoherentAtomSize` 并冻结，之后
//! 按角色规格在 `memoryTypeBits` 允许的类型里挑内存类型。必需属性给不出就报错，不悄悄
//! 降级；集成显卡的 DEVICE_LOCAL 本来就可能宿主可见，那是设备事实，不算降级。

use ash::{vk, Instance};

use crate::common::error::{Result, VulkanError};

/// 向上取整到 2 的幂对齐（绑定偏移、池内 bump 偏移、atom 取整共用）。
#[must_use]
pub fn align_up(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment.is_power_of_two(), "对齐必须是 2 的幂");
    (value + (alignment - 1)) & !(alignment - 1)
}

/// 设备内存事实的快照。内容恒定，克隆即拷贝：原件住 [`crate::render::Context`]，
/// 池和上传器各持一份。
#[derive(Clone)]
pub struct MemoryContract {
    properties: vk::PhysicalDeviceMemoryProperties,
    non_coherent_atom_size: u64,
}

impl MemoryContract {
    /// 查询并冻结选中物理设备的内存类型/堆表与 atom 粒度。
    ///
    /// # Safety
    /// `physical_device` 须是 `instance` 枚举出的合法句柄且未销毁。
    #[must_use]
    pub unsafe fn new(instance: &Instance, physical_device: vk::PhysicalDevice) -> Self {
        let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let non_coherent_atom_size =
            unsafe { instance.get_physical_device_properties(physical_device) }
                .limits
                .non_coherent_atom_size;
        Self {
            properties,
            non_coherent_atom_size,
        }
    }

    /// 非 coherent 宿主内存的 flush/invalidate 粒度（规范保证是 2 的幂）。
    #[must_use]
    pub fn non_coherent_atom_size(&self) -> u64 {
        self.non_coherent_atom_size
    }

    /// 在 `type_bits` 允许的类型里选：必需属性全含，优先属性命中最多，同分取小索引。
    ///
    /// # Errors
    /// 没有任何允许类型同时含全部必需属性时返回 [`VulkanError::Init`]，带上需要什么、
    /// 优先什么和设备允许位。
    pub fn find_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<u32> {
        let mut best: Option<(u32, u32)> = None; // (优先位命中数, 类型索引)
        for ty in 0..self.properties.memory_type_count {
            if type_bits & (1 << ty) == 0 {
                continue;
            }
            let flags = self.properties.memory_types[ty as usize].property_flags;
            if !flags.contains(required) {
                continue;
            }
            let score = (flags & preferred).as_raw().count_ones();
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, ty));
            }
        }
        best.map(|(_, ty)| ty).ok_or_else(|| {
            VulkanError::Init(format!(
                "内存契约无解:需要 {required:?} 优先 {preferred:?},设备允许位 {type_bits:#010b} \
                 的 {count} 个类型里没有全部必需属性的组合",
                count = self.properties.memory_type_count,
            ))
        })
    }

    /// 内存类型的属性位。
    #[must_use]
    pub fn type_properties(&self, ty: u32) -> vk::MemoryPropertyFlags {
        self.properties.memory_types[ty as usize].property_flags
    }

    /// 内存类型所在堆的索引。
    #[must_use]
    pub fn heap_index_of_type(&self, ty: u32) -> u32 {
        self.properties.memory_types[ty as usize].heap_index
    }

    /// 内存类型所在的堆（日志与验收报告用）。
    pub fn heap_of_type(&self, ty: u32) -> vk::MemoryHeap {
        self.properties.memory_heaps[self.heap_index_of_type(ty) as usize]
    }
}
