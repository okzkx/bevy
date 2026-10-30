//! 按角色规格创建并绑定的 buffer，全库 buffer 的唯一形态。
//!
//! - 恰按 `requirements.size` 分配一块内存、偏移 0 绑定；宿主可见的从创建映射到销毁，
//!   staging 复用时不重复 map。
//! - 内存不是 coherent 时，`write` 写完自动 flush、`read` 读前自动 invalidate，范围按
//!   `nonCoherentAtomSize` 向两端取整；unmap 不会隐式 flush，所以 flush 只收在 `write` 里。
//! - 不防 GPU 在飞读写：映射指针随时可写，"等 GPU 用完再写、再销毁"由提交侧的票据和
//!   帧槽 fence 负责。池内子分配、去重缓存和容量维护归调用方。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use super::contract::{align_up, MemoryContract};
use super::role::BufferRole;
use crate::common::error::{Result, VulkanError};

/// 按角色创建、恰量分配并绑定的 buffer；宿主可见的持久映射。
pub struct GpuBuffer {
    device: Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// 分配整体字节数 = `requirements.size`（可能大于请求值；映射和 flush 以此为界）
    allocation_size: u64,
    memory_type: u32,
    property_flags: vk::MemoryPropertyFlags,
    /// 持久映射指针（宿主可见才非空）
    mapped: *mut u8,
    non_coherent_atom_size: u64,
}

/// 非 coherent 内存的两种同步方向。
#[derive(Clone, Copy)]
enum HostSync {
    /// 宿主写完，让设备看得见。
    Flush,
    /// 宿主读之前，让设备写的内容对宿主可见。
    Invalidate,
}

impl GpuBuffer {
    /// 按角色创建，任何一步失败都回收已建对象。
    ///
    /// # Errors
    /// size 为 0、无满足角色规格的内存类型，或 Vulkan 创建/分配/绑定/映射失败。
    pub fn create(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        role: BufferRole,
    ) -> Result<Self> {
        Self::create_with_families(device, contract, size, role, &[])
    }

    /// `families` 给两个不同队列族时按 CONCURRENT 创建，否则 EXCLUSIVE。有专用 transfer
    /// 族的设备上，顶点/索引大池走 CONCURRENT：池被 transfer 反复写、图形长期读，省掉
    /// 每批上传的队列所有权转移；写 → 读的可见性仍靠票据信号量保证。
    ///
    /// # Errors
    /// 同 [`Self::create`]。
    pub fn create_with_families(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        role: BufferRole,
        families: &[u32],
    ) -> Result<Self> {
        if size == 0 {
            return Err(VulkanError::Init(
                "内存契约:size 必须大于 0(VkBufferCreateInfo::size 为 0 非法)".into(),
            ));
        }
        debug_assert!(
            families.len() < 2 || families[0] != families[1],
            "CONCURRENT 族集合须是两个不同族(同族设备走 EXCLUSIVE,调用方负责分流)"
        );
        let sharing_mode = if families.len() >= 2 {
            vk::SharingMode::CONCURRENT
        } else {
            vk::SharingMode::EXCLUSIVE
        };
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(role.spec().usage)
                    .sharing_mode(sharing_mode)
                    .queue_family_indices(families),
                None,
            )
        }?;
        let result = unsafe { Self::create_inner(device, contract, size, role, buffer) };
        // buffer 只在这里回收，内层只管 memory，避免重复销毁
        if result.is_err() {
            unsafe { device.destroy_buffer(buffer, None) };
        }
        result
    }

    /// 创建链的后半段（buffer 已建）：失败时回收已分配的 memory。
    ///
    /// # Safety
    /// `buffer` 须是刚创建成功的合法句柄；错误返回后调用方不得再使用它。
    unsafe fn create_inner(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        role: BufferRole,
        buffer: vk::Buffer,
    ) -> Result<Self> {
        unsafe {
            let reqs = device.get_buffer_memory_requirements(buffer);
            let spec = role.spec();
            let memory_type =
                contract.find_type(reqs.memory_type_bits, spec.required, spec.preferred)?;
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

    /// 分配整体字节数（requirements.size；可能大于创建时请求的 size）。
    #[must_use]
    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }

    /// 绑定的内存类型索引。
    #[must_use]
    pub fn memory_type(&self) -> u32 {
        self.memory_type
    }

    /// 绑定内存类型的属性位。
    #[must_use]
    pub fn memory_properties(&self) -> vk::MemoryPropertyFlags {
        self.property_flags
    }

    /// 是否宿主可见（可 write/read）。
    #[must_use]
    pub fn host_visible(&self) -> bool {
        self.property_flags
            .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
    }

    /// 是否宿主一致（读写免 flush/invalidate）。
    #[must_use]
    pub fn host_coherent(&self) -> bool {
        self.property_flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT)
    }

    /// 宿主写一段，越界报错不写。`&mut self` 保证同一映射不会被并发写。
    ///
    /// # Errors
    /// buffer 非宿主可见，或写入范围越出分配整体。
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.check_host_access(offset, bytes.len() as u64, "写")?;
        if bytes.is_empty() {
            return Ok(());
        }
        // # Safety:映射覆盖 [0, allocation_size)，范围刚检查过
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.mapped.add(offset as usize),
                bytes.len(),
            );
        }
        self.host_sync(offset, bytes.len() as u64, HostSync::Flush)
    }

    /// 宿主读一段，越界报错不读。
    ///
    /// # Errors
    /// buffer 非宿主可见，或读取范围越出分配整体。
    pub fn read(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check_host_access(offset, out.len() as u64, "读")?;
        if out.is_empty() {
            return Ok(());
        }
        self.host_sync(offset, out.len() as u64, HostSync::Invalidate)?;
        // # Safety:同 write
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mapped.add(offset as usize),
                out.as_mut_ptr(),
                out.len(),
            );
        }
        Ok(())
    }

    /// 宿主访问前置检查：必须宿主可见；非空范围 `[offset, offset+len)` 必须整体落在
    /// 分配内（checked_add 同时防 u64 溢出）。
    fn check_host_access(&self, offset: u64, len: u64, what: &str) -> Result<()> {
        if !self.host_visible() {
            return Err(VulkanError::Upload(format!(
                "host {what}要求 HOST_VISIBLE,本 buffer 类型属性 {:?}",
                self.property_flags
            )));
        }
        if len > 0 && offset
            .checked_add(len)
            .is_none_or(|end| end > self.allocation_size)
        {
            return Err(VulkanError::Capacity(format!(
                "host {what}越界:[{offset}, {}) 超出分配 {}B",
                offset.saturating_add(len),
                self.allocation_size
            )));
        }
        Ok(())
    }

    /// 非 coherent 内存的 flush/invalidate；coherent 内存直接跳过。
    ///
    /// 范围向两端取整到 atom 边界：只会扩到含读写字节的两个边界 atom，不会包进别的
    /// atom；终点取整后压回分配末尾，规范允许延伸到末尾的 size 不是 atom 倍数。
    fn host_sync(&self, offset: u64, len: u64, direction: HostSync) -> Result<()> {
        if self.host_coherent() || len == 0 {
            return Ok(());
        }
        let atom = self.non_coherent_atom_size.max(1);
        let start = offset & !(atom - 1);
        let end = align_up(offset + len, atom).min(self.allocation_size);
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .offset(start)
            .size(end - start);
        let ranges = slice::from_ref(&range);
        // # Safety:memory 处于本 buffer 的持久映射中，范围在映射内且 offset 已按 atom 对齐
        unsafe {
            match direction {
                HostSync::Flush => self.device.flush_mapped_memory_ranges(ranges),
                HostSync::Invalidate => self.device.invalidate_mapped_memory_ranges(ranges),
            }
        }?;
        Ok(())
    }
}

/// flush 策略的日志文案。
fn flush_policy(flags: vk::MemoryPropertyFlags) -> &'static str {
    if flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
        "HOST_COHERENT:免 flush/invalidate"
    } else if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
        "非 coherent:写后 flush/读前 invalidate,按 atom 对齐"
    } else {
        "纯设备内存:无宿主映射"
    }
}

/// # Safety
/// 映射指针只经 `write`/`read`（`&mut self`）触达，同一映射的宿主访问在类型层不可能
/// 并发：`Send` 只是整体移交所有权；`Sync` 下 `&GpuBuffer` 只暴露不解引用指针的查询口。
/// GPU 在飞读写的竞争归提交侧票据纪律，与线程安全无关。
unsafe impl Send for GpuBuffer {}
unsafe impl Sync for GpuBuffer {}

impl Drop for GpuBuffer {
    fn drop(&mut self) {
        // 不等 GPU：调用方必须在销毁前确认最后一次使用已完成
        unsafe {
            if !self.mapped.is_null() {
                self.device.unmap_memory(self.memory);
            }
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
