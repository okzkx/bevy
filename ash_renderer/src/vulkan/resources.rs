//! 内存契约(3.2.2 资源模块第一件,任务号 3.2.2.1):memory type、usage、绑定
//! 要求与对齐在一处定案,池/缓存/维护只引用不另立规则。
//!
//! 契约的设备侧事实一次查询冻结([`MemoryContract`]):内存类型/堆表 +
//! `nonCoherentAtomSize`。三条支柱:
//!
//! 1. **内存类型**:按角色([`BufferRole`])给定"必需属性 + 优先属性",在
//!    `VkMemoryRequirements::memoryTypeBits` 允许的类型里选——必需不满足即契约
//!    无效(报错冒泡,不静默降级);必需之外按优先位命中数取最优(同分取小索引)。
//!    staging 的 HOST_VISIBLE 是硬契约;HOST_COHERENT 是优先项,设备没有时走
//!    atom 对齐 flush(见下)。集成显卡的 DEVICE_LOCAL 本就允许宿主可见,属设备
//!    事实,不需要也不允许我们做"可见性降级"。
//! 2. **usage**:角色 → usage 一览定案(DevicePool = 顶点/索引读 + transfer 写;
//!    Staging = transfer 源;Readback = transfer 目标),创建入口只引用不重述。
//! 3. **绑定与对齐**:创建 → 查 requirements(size/alignment/typeBits)→ 按契约
//!    选类型 → 恰按 `requirements.size` 分配 → offset 0 绑定。四条绑定条款全部被
//!    该流程满足:memoryOffset(=0)小于 memory 大小(VUID-01031)、类型在
//!    memoryTypeBits 内(VUID-01035)、offset 是 alignment 整数倍(VUID-01036)、
//!    requirements.size ≤ memory 大小(VUID-01037)。子分配(bump 偏移)归 3.2.2.2
//!    的池,那里复用本模块的 [`align_up`]。
//!
//! **coherent 分支**(staging/readback 是否需要 flush/invalidate 的定案):
//! - 类型带 `HOST_COHERENT`:宿主读写设备侧立即可见,**不 flush**——规范明言此时
//!   flush/invalidate"非必需且有性能代价"(memory.adoc §Host Access 之 Note)。
//! - 不带:宿主写后 [`GpuBuffer::write`] 自动 flush,宿主读前 [`GpuBuffer::read`]
//!   自动 invalidate;范围按 `nonCoherentAtomSize` 向两端舍入(memory.adoc
//!   vkMapMemory 段:无 coherent 时 hazard 保证必须覆盖舍入后的范围)。舍入只波及
//!   **含数据字节的边界 atom**——连续写区向两端取整不会包进任何未写 atom,满足
//!   "未写的 atom 不得 flush"条款;显式 size 为 0 的范围直接跳过(offset/size 的
//!   atom 条款连空范围都约束,跳过最干净)。unmap 不隐式 flush(memory.adoc
//!   §memory-device-unmap-does-not-flush),flush 纪律收在 write 里,调用方没有
//!   "忘记 flush"的入口。
//!
//! 持久映射:host-visible 分配在创建时 map 一次、Drop 时 unmap——map/unmap 次数
//! 与 buffer 寿命一致,3.2.4 的 staging 复用不逐批重映射。
//!
//! 本模块**不做**的事与归属:池/去重缓存(3.2.2.2)、容量维护与销毁等待(3.2.2.3)、
//! transfer 提交与票据(3.2.4)。`vkMapMemory` 不检查在用即返回指针——host 写与
//! GPU 读的竞争防护是提交侧的内存依赖/票据纪律,不在读写 API 里设防(边界见
//! 施工记录 §边界)。

use std::slice;

use ash::{vk, Device, Instance};
use bevy::log::info;

use crate::error::VulkanError;

/// buffer 用途角色:usage 与 memory 属性要求在此定案,创建入口逐一引用。
///
/// | 角色 | usage | 必需属性 | 优先属性 |
/// |---|---|---|---|
/// | `DevicePool` | VERTEX_BUFFER+INDEX_BUFFER+TRANSFER_DST | DEVICE_LOCAL | — |
/// | `Staging` | TRANSFER_SRC | HOST_VISIBLE | HOST_COHERENT |
/// | `Readback` | TRANSFER_DST | HOST_VISIBLE | HOST_CACHED+HOST_COHERENT |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferRole {
    /// 顶点/索引大池本体:图形阶段读,3.2.4 的 transfer 提交写(TRANSFER_DST)。
    /// DEVICE_LOCAL 是定案(施工计划 §2 池策略),不给 host-visible 降级路径。
    DevicePool,
    /// 上传暂存:宿主写 + transfer 读。HOST_VISIBLE 硬契约(HOST_COHERENT 优先)。
    Staging,
    /// 回读验证:transfer 写 + 宿主读(HOST_CACHED 优先,加速宿主读侧)。
    Readback,
}

impl BufferRole {
    /// 角色对应的 buffer usage(创建 buffer 时的唯一引用点)。
    #[must_use]
    pub fn usage(self) -> vk::BufferUsageFlags {
        match self {
            Self::DevicePool => {
                vk::BufferUsageFlags::VERTEX_BUFFER
                    | vk::BufferUsageFlags::INDEX_BUFFER
                    | vk::BufferUsageFlags::TRANSFER_DST
                    // 迁移/扩容与回读校验要把池内容拷出去:3.2.2.3 的维护路径与
                    // 3.2 的内容抽查验证都以池为拷贝源,TRANSFER_SRC 一并冻结
                    | vk::BufferUsageFlags::TRANSFER_SRC
            }
            Self::Staging => vk::BufferUsageFlags::TRANSFER_SRC,
            Self::Readback => vk::BufferUsageFlags::TRANSFER_DST,
        }
    }

    /// 角色对内存类型的必需属性(不满足即契约无解,报错不降级)。
    #[must_use]
    pub fn required_memory(self) -> vk::MemoryPropertyFlags {
        match self {
            Self::DevicePool => vk::MemoryPropertyFlags::DEVICE_LOCAL,
            Self::Staging | Self::Readback => vk::MemoryPropertyFlags::HOST_VISIBLE,
        }
    }

    /// 必需之外的优先属性:命中越多越优,纯选择偏好,不构成否决。
    #[must_use]
    pub fn preferred_memory(self) -> vk::MemoryPropertyFlags {
        match self {
            Self::DevicePool => vk::MemoryPropertyFlags::empty(),
            Self::Staging => vk::MemoryPropertyFlags::HOST_COHERENT,
            Self::Readback => {
                vk::MemoryPropertyFlags::HOST_CACHED | vk::MemoryPropertyFlags::HOST_COHERENT
            }
        }
    }
}

/// 向上取整到 2 的幂对齐(绑定偏移、bump 偏移与 atom 舍入共用)。
#[must_use]
pub fn align_up(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment.is_power_of_two(), "对齐必须是 2 的幂");
    (value + (alignment - 1)) & !(alignment - 1)
}

/// 内存契约:设备侧不可变事实(内存类型/堆表 + `nonCoherentAtomSize`)创建时一次
/// 查询冻结,类型选择的唯一裁判。
pub struct MemoryContract {
    properties: vk::PhysicalDeviceMemoryProperties,
    non_coherent_atom_size: u64,
}

impl MemoryContract {
    /// 查询并冻结选中物理设备的内存类型/堆表与 atom 粒度。
    ///
    /// # Safety
    /// `physical_device` 须是 `instance` 枚举出的合法句柄且未销毁(ash 约定:
    /// 所有 Vulkan 命令的参数合法性由调用方担保)。
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

    /// 非 coherent 宿主内存的并发访问粒度(规范要求 2 的幂,上限 256)。
    #[must_use]
    pub fn non_coherent_atom_size(&self) -> u64 {
        self.non_coherent_atom_size
    }

    /// 在 `type_bits` 允许的类型里选:必需属性全含 + 优先属性命中数最多,同分取
    /// 小索引(类型序即设备报告序)。无解 = 契约被设备打破,带全部证据报 `Init`
    /// (需要什么、优先什么、设备给过什么),不静默降级。
    ///
    /// # Errors
    /// 没有任何允许类型同时含全部必需属性时返回 [`VulkanError::Init`]。
    pub fn find_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<u32, VulkanError> {
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

    /// 选中类型的属性位(证据查询口)。
    #[must_use]
    pub fn type_properties(&self, ty: u32) -> vk::MemoryPropertyFlags {
        self.properties.memory_types[ty as usize].property_flags
    }

    /// 类型所在堆的索引。
    #[must_use]
    pub fn heap_index_of_type(&self, ty: u32) -> u32 {
        self.properties.memory_types[ty as usize].heap_index
    }

    /// 类型所在堆(日志与验收报告用)。
    pub fn heap_of_type(&self, ty: u32) -> vk::MemoryHeap {
        self.properties.memory_heaps[self.heap_index_of_type(ty) as usize]
    }
}

/// 一个按契约创建并绑定的 buffer:恰按 `requirements.size` 分配、offset 0 绑定、
/// host-visible 则持久映射。pool/bump/缓存是 3.2.2.2 的事,不以本类型扩池。
pub struct GpuBuffer {
    device: Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// 分配整体字节数 = `requirements.size`(可能大于请求值;flush/映射以此为界)
    allocation_size: u64,
    memory_type: u32,
    property_flags: vk::MemoryPropertyFlags,
    /// 持久映射指针(host-visible 才非空);map 一次随 buffer 寿命,Drop 时 unmap
    mapped: *mut u8,
    non_coherent_atom_size: u64,
}

impl GpuBuffer {
    /// 按契约创建:buffer(usage 按角色)→ requirements → 选型 → 恰量分配 →
    /// offset 0 绑定 → host-visible 则持久映射。任何失败回收已建对象(全有或全无)。
    ///
    /// # Errors
    /// size 为 0、无满足契约的内存类型,或 Vulkan 创建/分配/绑定/映射失败。
    pub fn create(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        role: BufferRole,
    ) -> Result<Self, VulkanError> {
        if size == 0 {
            return Err(VulkanError::Init(
                "内存契约:size 必须大于 0(VkBufferCreateInfo::size 为 0 非法)".into(),
            ));
        }
        // # Safety:create_buffer 参数全为合法默认 + 契约 usage,无裸指针
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(role.usage()),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(device, contract, size, role, buffer) };
        // buffer 的回收统一收口在这里(内层只管 memory 的回收),避免双重销毁
        if result.is_err() {
            unsafe { device.destroy_buffer(buffer, None) };
        }
        result
    }

    /// 创建链的后半段(buffer 已建):失败路径回收已分配的 memory。
    ///
    /// # Safety
    /// `buffer` 须是刚创建成功的合法句柄;错误返回后调用方不得再使用它。
    unsafe fn create_inner(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        role: BufferRole,
        buffer: vk::Buffer,
    ) -> Result<Self, VulkanError> {
        unsafe {
            // 绑定四条款的证据来源:requirements 三元组,一次查询全用上
            let reqs = device.get_buffer_memory_requirements(buffer);
            let memory_type = contract.find_type(
                reqs.memory_type_bits,
                role.required_memory(),
                role.preferred_memory(),
            )?;
            let property_flags = contract.type_properties(memory_type);
            let memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            if let Err(e) = device.bind_buffer_memory(buffer, memory, 0) {
                device.free_memory(memory, None);
                return Err(VulkanError::Vk(e));
            }
            // 持久映射:host-visible 分配 map 一次到 Drop(WHOLE_SIZE 自 offset 0
            // 起 = 映射整个分配,后续 flush/invalidate 的显式范围都落在此映射内)
            let mapped = if property_flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
                match device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) {
                    Ok(ptr) => ptr.cast::<u8>(),
                    Err(e) => {
                        device.free_memory(memory, None);
                        return Err(VulkanError::Vk(e));
                    }
                }
            } else {
                std::ptr::null_mut()
            };
            let heap = contract.heap_of_type(memory_type);
            info!(
                "{role:?} buffer 按契约就绪: 请求 {size}B → requirements 分配 {reqs_size}B(对齐 {reqs_alignment}B), \
                 内存类型 {memory_type}({property_flags:?}) 堆 {heap_index}({heap_size}B, {heap_flags:?}), \
                 flush 策略 {policy}",
                reqs_size = reqs.size,
                reqs_alignment = reqs.alignment,
                heap_index = contract.heap_index_of_type(memory_type),
                heap_size = heap.size,
                heap_flags = heap.flags,
                policy = flush_policy(property_flags),
            );
            Ok(Self {
                device: device.clone(),
                buffer,
                memory,
                allocation_size: reqs.size,
                memory_type,
                property_flags,
                mapped,
                non_coherent_atom_size: contract.non_coherent_atom_size(),
            })
        }
    }

    /// 绑定的 Vulkan buffer 句柄。
    #[must_use]
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// 分配整体字节数(requirements.size;可能大于创建时请求的 size)。
    #[must_use]
    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }

    /// 绑定的内存类型索引(证据查询口)。
    #[must_use]
    pub fn memory_type(&self) -> u32 {
        self.memory_type
    }

    /// 绑定内存类型的属性位(证据查询口)。
    #[must_use]
    pub fn memory_properties(&self) -> vk::MemoryPropertyFlags {
        self.property_flags
    }

    /// 是否宿主可见(可 write/read)。
    #[must_use]
    pub fn host_visible(&self) -> bool {
        self.property_flags
            .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
    }

    /// 是否宿主一致(读写免 flush/invalidate)——coherent 分支的定案事实。
    #[must_use]
    pub fn host_coherent(&self) -> bool {
        self.property_flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT)
    }

    /// 宿主写一段:拷入映射区;非 coherent 时按 atom 对齐 flush(写区向两端舍入,
    /// 只波及含写入字节的边界 atom)。越界一律报错,绝不越界写。
    ///
    /// # Errors
    /// buffer 非 host-visible,或写入范围越出分配整体。
    pub fn write(&self, offset: u64, bytes: &[u8]) -> Result<(), VulkanError> {
        if !self.host_visible() {
            return Err(VulkanError::Init(format!(
                "host 写要求 HOST_VISIBLE,本 buffer 类型属性 {:?}",
                self.property_flags
            )));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let len = bytes.len() as u64;
        self.check_range(offset, len, "写")?;
        // # Safety:pointer 来自成功的 map(映射 [0, allocation_size) 全程),
        // 上一行刚验证 offset+len 不出分配整体,拷贝不会越界;len > 0 已挡空段
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.mapped.add(offset as usize),
                bytes.len(),
            );
        }
        if !self.host_coherent() {
            self.coherent_flush(offset, len)?;
        }
        Ok(())
    }

    /// 宿主读一段:非 coherent 时先按 atom 对齐 invalidate(设备写对宿主可见),
    /// 再拷出。与 `write` 一样不做 GPU 在飞防护——那是提交侧票据纪律。
    ///
    /// # Errors
    /// buffer 非 host-visible,或读取范围越出分配整体。
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), VulkanError> {
        if !self.host_visible() {
            return Err(VulkanError::Init(format!(
                "host 读要求 HOST_VISIBLE,本 buffer 类型属性 {:?}",
                self.property_flags
            )));
        }
        if out.is_empty() {
            return Ok(());
        }
        let len = out.len() as u64;
        self.check_range(offset, len, "读")?;
        if !self.host_coherent() {
            self.coherent_invalidate(offset, len)?;
        }
        // # Safety:同 write——映射覆盖 [0, allocation_size),范围已验证
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mapped.add(offset as usize),
                out.as_mut_ptr(),
                out.len(),
            );
        }
        Ok(())
    }

    /// 范围前置检查:`[offset, offset+len)` 须整体落在分配内(checked_add 同时
    /// 防 u64 溢出),不满足即报错——契约的"不越界"在指针运算之前执行。
    fn check_range(&self, offset: u64, len: u64, what: &str) -> Result<(), VulkanError> {
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.allocation_size)
        {
            return Err(VulkanError::Init(format!(
                "host {what}越界:[{offset}, {}) 超出分配 {}B",
                offset.saturating_add(len),
                self.allocation_size
            )));
        }
        Ok(())
    }

    /// 写侧 flush:`[offset, offset+len)` 向两端舍入到 atom 边界后 flush。
    /// 舍入只扩到含写入字节的两个边界 atom,未写 atom 永不进范围。
    fn coherent_flush(&self, offset: u64, len: u64) -> Result<(), VulkanError> {
        let Some((start, size)) = self.atom_range(offset, len) else {
            return Ok(());
        };
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .offset(start)
            .size(size);
        // # Safety:memory 处于本 buffer 的持久映射中,范围在映射内且 offset 已按
        // atom 对齐(VUID-00684/00685/00687/01390 的前置全部成立)
        unsafe {
            self.device
                .flush_mapped_memory_ranges(slice::from_ref(&range))
        }?;
        Ok(())
    }

    /// 读侧 invalidate:与 flush 同一套舍入与跳过规则。
    fn coherent_invalidate(&self, offset: u64, len: u64) -> Result<(), VulkanError> {
        let Some((start, size)) = self.atom_range(offset, len) else {
            return Ok(());
        };
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .offset(start)
            .size(size);
        // # Safety:同 coherent_flush,映射与 atom 前置均成立
        unsafe {
            self.device
                .invalidate_mapped_memory_ranges(slice::from_ref(&range))
        }?;
        Ok(())
    }

    /// atom 对齐的 flush/invalidate 范围:len 为 0 返回 None(空范围不送 API);
    /// 终点先压回分配整体,再向上取整——终点等于分配整体时 size 非 atom 倍数
    /// 也是条款允许的"延伸到 memory 末尾"分支(VUID-01390)。
    fn atom_range(&self, offset: u64, len: u64) -> Option<(u64, u64)> {
        if len == 0 {
            return None;
        }
        let atom = self.non_coherent_atom_size.max(1);
        let start = offset & !(atom - 1);
        let end = (offset + len).min(self.allocation_size);
        Some((start, align_up(end, atom).min(self.allocation_size) - start))
    }
}

/// flush 策略的日志文案(coherent 分支定案的三种形态)。
fn flush_policy(flags: vk::MemoryPropertyFlags) -> &'static str {
    if flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
        "HOST_COHERENT:免 flush/invalidate"
    } else if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
        "非 coherent:写后 flush/读前 invalidate,按 atom 对齐"
    } else {
        "纯设备内存:无宿主映射"
    }
}

impl Drop for GpuBuffer {
    fn drop(&mut self) {
        // 契约边界:本 Drop 不等 GPU 在飞使用——"等待最后使用完成再销毁"是
        // 3.2.2.3 的容量/销毁账本与 3.2.4.4 的退出排空纪律;调用方(含探针)
        // 必须在销毁前自证最后一次使用已完成。unmap 显式先行(vkFreeMemory
        // 虽会隐式 unmap,写出顺序让依赖一目了然),再拆 buffer、还 memory。
        unsafe {
            if !self.mapped.is_null() {
                self.device.unmap_memory(self.memory);
            }
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
