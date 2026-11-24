//! 每帧 TLAS：按帧槽各一份 AS + 实例 buffer 的构建尺寸 + scratch。实例数组本体住在
//! `InstanceTables`（set2 b11，compute 每帧写），本模块只管加速结构对象与构建命令。
//!
//! 更新策略见施工计划 §0.3：**每帧全量重建**（动态场景是前提），容量按在场 high-water
//! 向上取整、不足时重建（先等全部在飞帧）。实例引用 0 合法（VUID-12281），缺席槽不造句化 BLAS。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::context::Context;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::align_up;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::record::barrier::MemoryBarrierDesc;
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 首建容量：即使空场景也要给 RT 描述符集一个合法 TLAS 句柄（0 实例的 AS 不合法）。
const INITIAL_CAPACITY: u32 = 1024;

struct TlasSlot {
    as_: vk::AccelerationStructureKHR,
    /// AS 引用这块内存（保活）。
    storage: GpuBuffer,
    scratch: GpuBuffer,
    scratch_address: vk::DeviceAddress,
    /// 可容纳的实例数上界。
    capacity: u32,
}

/// 每帧槽一份 TLAS。
pub struct TlasFrame {
    device: Device,
    accel: ash::khr::acceleration_structure::Device,
    slots: Vec<TlasSlot>,
}

impl TlasFrame {
    /// 建初始容量（[`INITIAL_CAPACITY`]）的 TLAS 对象，每帧槽一份。
    ///
    /// # Errors
    /// 尺寸查询、storage/scratch 分配或 AS 创建失败。
    pub(crate) fn new(
        ctx: &Context,
        accel: &ash::khr::acceleration_structure::Device,
        properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
    ) -> Result<Self> {
        let mut this = Self {
            device: ctx.device.clone(),
            accel: accel.clone(),
            slots: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
        };
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let slot = this.create_slot(ctx, properties, INITIAL_CAPACITY)?;
            this.slots.push(slot);
        }
        Ok(this)
    }

    /// 建一份容量 `capacity` 的 TLAS（storage + scratch + AS 对象）。
    fn create_slot(
        &self,
        ctx: &Context,
        properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
        capacity: u32,
    ) -> Result<TlasSlot> {
        let instances = vk::AccelerationStructureGeometryInstancesDataKHR::default()
            .array_of_pointers(false)
            .data(vk::DeviceOrHostAddressConstKHR { device_address: 0 });
        let geometry = vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::INSTANCES)
            .geometry(vk::AccelerationStructureGeometryDataKHR { instances });
        let build_info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
            .flags(vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_BUILD)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(std::slice::from_ref(&geometry));
        let mut sizes = vk::AccelerationStructureBuildSizesInfoKHR::default();
        unsafe {
            self.accel.get_acceleration_structure_build_sizes(
                vk::AccelerationStructureBuildTypeKHR::DEVICE,
                &build_info,
                &[capacity],
                &mut sizes,
            );
        }
        if sizes.acceleration_structure_size == 0 || sizes.build_scratch_size == 0 {
            return Err(VulkanError::Contract(format!(
                "TLAS 构建尺寸为 0（容量 {capacity} 实例）——设备属性异常"
            )));
        }
        let storage = GpuBuffer::create(
            &self.device,
            ctx.memory_contract(),
            sizes.acceleration_structure_size,
            BufferRole::AccelerationStructureStorage,
        )?;
        let as_ = unsafe {
            self.accel.create_acceleration_structure(
                &vk::AccelerationStructureCreateInfoKHR::default()
                    .buffer(storage.buffer())
                    .offset(0)
                    .size(sizes.acceleration_structure_size)
                    .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("创建 TLAS 对象失败: {e}")))?;
        let scratch_align =
            u64::from(properties.min_acceleration_structure_scratch_offset_alignment).max(1);
        let scratch = GpuBuffer::create(
            &self.device,
            ctx.memory_contract(),
            sizes.build_scratch_size + scratch_align,
            BufferRole::AccelerationStructureScratch,
        )?;
        let base = ctx.buffer_address(scratch.buffer());
        let scratch_address = align_up(base, scratch_align) as vk::DeviceAddress;
        if scratch_address - base + sizes.build_scratch_size > scratch.allocation_size() {
            unsafe { self.accel.destroy_acceleration_structure(as_, None) };
            return Err(VulkanError::Contract(
                "TLAS scratch 对齐窗口越界（分配大小不足）".into(),
            ));
        }
        Ok(TlasSlot {
            as_,
            storage,
            scratch,
            scratch_address,
            capacity,
        })
    }

    /// 容量保证：本帧实例数超出现容量时，等全部在飞帧后按新容量重建**每个帧槽**的
    /// TLAS（两帧槽各自 build/读，一帧的改写不能掀翻另一帧正在读的结构）。
    ///
    /// # Errors
    /// 重建失败（旧结构保持不变，调用方随之致命退出）。
    pub(crate) fn ensure_capacity(
        &mut self,
        ctx: &Context,
        properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
        needed: u32,
        wait: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if needed == 0 || self.slots.iter().all(|s| s.capacity >= needed) {
            return Ok(());
        }
        let capacity = next_capacity(needed);
        wait()?;
        // 先全部建好再换：create_slot 只借 &self，换绑循环里不回调它
        let mut fresh = Vec::with_capacity(self.slots.len());
        for _ in 0..self.slots.len() {
            fresh.push(self.create_slot(ctx, properties, capacity)?);
        }
        for (slot, new_slot) in self.slots.iter_mut().zip(fresh) {
            let old = std::mem::replace(slot, new_slot);
            // 先销毁 AS 再释放它引用的 buffer
            unsafe { self.accel.destroy_acceleration_structure(old.as_, None) };
            drop(old.storage);
            drop(old.scratch);
        }
        Ok(())
    }

    /// 第 `slot` 帧槽的 TLAS 句柄（RT 描述符集 b0）。
    #[must_use]
    pub(crate) fn handle(&self, slot: usize) -> vk::AccelerationStructureKHR {
        self.slots[slot].as_
    }

    #[must_use]
    pub(crate) fn capacity(&self, slot: usize) -> u32 {
        self.slots[slot].capacity
    }

    /// 两帧槽 TLAS 的 storage 字节合计（BRP 读数）。
    #[must_use]
    pub(crate) fn bytes(&self) -> u64 {
        self.slots.iter().map(|s| s.storage.allocation_size()).sum()
    }

    /// 录 TLAS 构建命令：实例数组（compute 本帧写）→ 屏障 → `cmd_build_acceleration_structures`
    /// → 屏障（构建写 → compute/ray query 读）。
    pub(crate) fn record_build(
        &self,
        cb: vk::CommandBuffer,
        slot: usize,
        instance_buffer_address: vk::DeviceAddress,
        instance_count: u32,
    ) {
        let slot_state = &self.slots[slot];
        debug_assert!(
            instance_count <= slot_state.capacity,
            "TLAS 容量保证未先执行（实例数超容量）"
        );
        // compute 写实例数组 → 构建命令读（经 BDA），两处都是 GPU 侧访问
        unsafe {
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                src_access: vk::AccessFlags::SHADER_WRITE,
                dst_stage: vk::PipelineStageFlags::ACCELERATION_STRUCTURE_BUILD_KHR,
                dst_access: vk::AccessFlags::ACCELERATION_STRUCTURE_READ_KHR
                    | vk::AccessFlags::SHADER_READ,
            }
            .record(&self.device, cb);
        }
        let instances = vk::AccelerationStructureGeometryInstancesDataKHR::default()
            .array_of_pointers(false)
            .data(vk::DeviceOrHostAddressConstKHR {
                device_address: instance_buffer_address,
            });
        let geometry = vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::INSTANCES)
            .geometry(vk::AccelerationStructureGeometryDataKHR { instances });
        let build_info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
            .flags(vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_BUILD)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .dst_acceleration_structure(slot_state.as_)
            .geometries(std::slice::from_ref(&geometry))
            .scratch_data(vk::DeviceOrHostAddressKHR {
                device_address: slot_state.scratch_address,
            });
        let range =
            vk::AccelerationStructureBuildRangeInfoKHR::default().primitive_count(instance_count);
        unsafe {
            self.accel.cmd_build_acceleration_structures(
                cb,
                std::slice::from_ref(&build_info),
                &[std::slice::from_ref(&range)],
            );
        }
        // 构建写 → 后续 compute 的 ray query 读
        unsafe {
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::ACCELERATION_STRUCTURE_BUILD_KHR,
                src_access: vk::AccessFlags::ACCELERATION_STRUCTURE_WRITE_KHR,
                dst_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                dst_access: vk::AccessFlags::ACCELERATION_STRUCTURE_READ_KHR
                    | vk::AccessFlags::SHADER_READ,
            }
            .record(&self.device, cb);
        }
    }
}

impl Drop for TlasFrame {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle
        for slot in self.slots.drain(..) {
            unsafe {
                self.accel.destroy_acceleration_structure(slot.as_, None);
            }
        }
    }
}

/// 新容量：需量 ×4/3，至少翻倍，按 1024 向上取整。
fn next_capacity(needed: u32) -> u32 {
    let target = needed
        .saturating_add(needed / 3)
        .max(needed.saturating_mul(2))
        .max(INITIAL_CAPACITY);
    align_up(u64::from(target), 1024) as u32
}

#[cfg(test)]
mod tests {
    use super::{next_capacity, INITIAL_CAPACITY};

    /// 容量只增且覆盖需量；小需量落到初始档。
    #[test]
    fn next_capacity_covers_need() {
        assert_eq!(next_capacity(1), INITIAL_CAPACITY);
        assert_eq!(next_capacity(INITIAL_CAPACITY), 2048);
        let c = next_capacity(5000);
        assert!(c >= 5000 && c.is_multiple_of(1024), "容量 {c} 不覆盖 5000");
    }
}
