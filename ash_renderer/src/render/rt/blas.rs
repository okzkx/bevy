//! BLAS 注册表：按 mesh 资产身份缓存加速结构，构建输入直接指 `MeshPool` 的顶点/索引区间
//!（不复制几何）。生命周期由驻留事件驱动——`ensure_build` 在资产锚定某行的记录写入时排建，
//! `retire` 在资产退场/换内容时排销毁；销毁要等在飞帧用完（[`BlasRegistry::drain`]）。
//!
//! 首版每个 BLAS 一条独立 storage buffer + AS 对象 + scratch buffer，offset 恒 0
//!（AS 存储 offset 必须 256 倍数，VUID-03734 天然满足）。合并构建：一次
//! `cmd_build_acceleration_structures` 带本帧全部待建条目，每条独立 scratch（不得共用同一段）。

use std::collections::HashMap;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::log::info;
use bevy::mesh::Mesh;

use crate::common::error::{Result, VulkanError};

use crate::render::core::device::context::Context;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::align_up;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::resident::assets::pool::MeshSlot;

/// 一个已建成的 BLAS：AS 对象、它引用的存储 buffer（保活）与设备地址。
struct BlasEntry {
    as_: vk::AccelerationStructureKHR,
    /// AS 引用这块内存，必须与 AS 同寿命（先销毁 AS 再释放 buffer）。
    storage: GpuBuffer,
    address: vk::DeviceAddress,
}

/// 一条待建 BLAS：构建输入（地址 + 计数）、scratch 与 AS 对象都已就绪，
/// 只差一帧 ② 段录构建命令。
struct PendingBuild {
    id: AssetId<Mesh>,
    as_: vk::AccelerationStructureKHR,
    storage: GpuBuffer,
    scratch: GpuBuffer,
    /// scratch 在 buffer 内的可用地址（按 minScratchOffsetAlignment 向上取整后）。
    scratch_address: vk::DeviceAddress,
    /// **池内偏移**而不是绝对地址：排建与录命令之间网格池可能扩容迁移（地址变、旧 buffer
    /// 销毁），绝对地址留到 `record_builds` 用当帧的池基址现拼（VUID-11364 实抓）。
    vertex_offset: u64,
    index_offset: u64,
    vertex_count: u32,
    primitive_count: u32,
}

/// BLAS 注册表。`generation` 在建成/销毁时递增，地址表据此决定要不要整表重写。
pub struct BlasRegistry {
    device: Device,
    accel: ash::khr::acceleration_structure::Device,
    entries: HashMap<AssetId<Mesh>, BlasEntry>,
    pending: Vec<PendingBuild>,
    /// 已排销毁的 (AS, storage)：等 `drain` 的 `wait` 之后才拆。
    retired: Vec<(vk::AccelerationStructureKHR, GpuBuffer)>,
    /// 构建命令用过的 scratch：录完的一帧还要执行，下一次 `drain` 才释放。
    scratch_graveyard: Vec<GpuBuffer>,
    generation: u64,
}

impl BlasRegistry {
    pub(crate) fn new(device: &Device, accel: &ash::khr::acceleration_structure::Device) -> Self {
        Self {
            device: device.clone(),
            accel: accel.clone(),
            entries: HashMap::new(),
            pending: Vec::new(),
            retired: Vec::new(),
            scratch_graveyard: Vec::new(),
            generation: 0,
        }
    }

    /// 排建一份 BLAS（幂等：已建成/已排队直接返回 false）。构建输入是 `MeshPool` 的区间。
    ///
    /// # Errors
    /// 构建尺寸查询失败、storage/scratch 分配失败、AS 对象创建失败。
    pub(crate) fn ensure_build(
        &mut self,
        ctx: &Context,
        properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
        id: AssetId<Mesh>,
        slot: &MeshSlot,
        vertex_buffer: vk::Buffer,
        index_buffer: vk::Buffer,
    ) -> Result<bool> {
        if self.entries.contains_key(&id) || self.pending.iter().any(|p| p.id == id) {
            return Ok(false);
        }
        if slot.index_count < 3 || slot.vertex_count == 0 {
            return Ok(false);
        }
        // 这里只用基址查构建尺寸；最终构建输入地址在 record_builds 用当帧基址现拼
        let vertex_address = ctx.buffer_address(vertex_buffer) + slot.vertex.offset;
        let index_address = ctx.buffer_address(index_buffer) + slot.index.offset;
        let primitive_count = slot.index_count / 3;
        let triangles = vk::AccelerationStructureGeometryTrianglesDataKHR::default()
            .vertex_format(vk::Format::R32G32B32_SFLOAT)
            .vertex_data(vk::DeviceOrHostAddressConstKHR {
                device_address: vertex_address,
            })
            .vertex_stride(crate::render::VERTEX_STRIDE)
            .max_vertex(slot.vertex_count - 1)
            .index_type(vk::IndexType::UINT32)
            .index_data(vk::DeviceOrHostAddressConstKHR {
                device_address: index_address,
            });
        let geometry = vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::TRIANGLES)
            .geometry(vk::AccelerationStructureGeometryDataKHR { triangles })
            .flags(vk::GeometryFlagsKHR::OPAQUE);
        let build_info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL)
            .flags(vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(std::slice::from_ref(&geometry));
        let mut sizes = vk::AccelerationStructureBuildSizesInfoKHR::default();
        unsafe {
            self.accel.get_acceleration_structure_build_sizes(
                vk::AccelerationStructureBuildTypeKHR::DEVICE,
                &build_info,
                &[primitive_count],
                &mut sizes,
            );
        }
        if sizes.acceleration_structure_size == 0 || sizes.build_scratch_size == 0 {
            return Err(VulkanError::Contract(format!(
                "BLAS 构建尺寸为 0（{id:?}，{primitive_count} 三角形）——几何区间形状可疑"
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
                    .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("创建 BLAS 对象失败（{id:?}）: {e}")))?;
        // scratch 多分配一个对齐窗口：偏移必须满足 minScratchOffsetAlignment（VUID-03708），
        // 缓冲基地址不保证满足，按取整后的地址切一段
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
        let scratch_offset = scratch_address - base;
        if scratch_offset + sizes.build_scratch_size > scratch.allocation_size() {
            unsafe { self.accel.destroy_acceleration_structure(as_, None) };
            return Err(VulkanError::Contract(
                "AS scratch 对齐窗口越界（分配大小不足）".into(),
            ));
        }
        info!(
            "BLAS 排建: {id:?} {primitive_count} 三角形，storage {}B + scratch {}B（build_scratch {}B）",
            sizes.acceleration_structure_size,
            scratch.allocation_size(),
            sizes.build_scratch_size,
        );
        self.pending.push(PendingBuild {
            id,
            as_,
            storage,
            scratch,
            scratch_address,
            vertex_offset: slot.vertex.offset,
            index_offset: slot.index.offset,
            vertex_count: slot.vertex_count,
            primitive_count,
        });
        Ok(true)
    }

    /// 录本帧全部待建 BLAS（一次 `cmd_build_acceleration_structures`），返回条数。
    /// 录完的 scratch 进停尸房（命令还要执行一帧），AS/storage 转正进缓存。
    ///
    /// `vertex_base`/`index_base` 是**本帧**网格池的设备基址（调用方现取）：排建到录制之间
    /// 池可能扩容迁移，用排建时的旧地址会指向已销毁的 buffer（VUID-11364，实抓设备丢失）。
    pub(crate) fn record_builds(
        &mut self,
        cb: vk::CommandBuffer,
        vertex_base: vk::DeviceAddress,
        index_base: vk::DeviceAddress,
    ) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let geoms: Vec<vk::AccelerationStructureGeometryKHR> = self
            .pending
            .iter()
            .map(|p| {
                let triangles = vk::AccelerationStructureGeometryTrianglesDataKHR::default()
                    .vertex_format(vk::Format::R32G32B32_SFLOAT)
                    .vertex_data(vk::DeviceOrHostAddressConstKHR {
                        device_address: vertex_base + p.vertex_offset,
                    })
                    .vertex_stride(crate::render::VERTEX_STRIDE)
                    .max_vertex(p.vertex_count - 1)
                    .index_type(vk::IndexType::UINT32)
                    .index_data(vk::DeviceOrHostAddressConstKHR {
                        device_address: index_base + p.index_offset,
                    });
                vk::AccelerationStructureGeometryKHR::default()
                    .geometry_type(vk::GeometryTypeKHR::TRIANGLES)
                    .geometry(vk::AccelerationStructureGeometryDataKHR { triangles })
                    .flags(vk::GeometryFlagsKHR::OPAQUE)
            })
            .collect();
        let infos: Vec<vk::AccelerationStructureBuildGeometryInfoKHR> = self
            .pending
            .iter()
            .zip(&geoms)
            .map(|(p, geometry)| {
                vk::AccelerationStructureBuildGeometryInfoKHR::default()
                    .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL)
                    .flags(vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE)
                    .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
                    .dst_acceleration_structure(p.as_)
                    .geometries(std::slice::from_ref(geometry))
                    .scratch_data(vk::DeviceOrHostAddressKHR {
                        device_address: p.scratch_address,
                    })
            })
            .collect();
        let ranges: Vec<vk::AccelerationStructureBuildRangeInfoKHR> = self
            .pending
            .iter()
            .map(|p| {
                vk::AccelerationStructureBuildRangeInfoKHR::default()
                    .primitive_count(p.primitive_count)
            })
            .collect();
        let range_slices: Vec<&[vk::AccelerationStructureBuildRangeInfoKHR]> =
            ranges.iter().map(std::slice::from_ref).collect();
        unsafe {
            self.accel
                .cmd_build_acceleration_structures(cb, &infos, &range_slices);
        }
        let count = self.pending.len();
        for p in self.pending.drain(..) {
            let address = unsafe {
                self.accel.get_acceleration_structure_device_address(
                    &vk::AccelerationStructureDeviceAddressInfoKHR::default()
                        .acceleration_structure(p.as_),
                )
            };
            self.entries.insert(
                p.id,
                BlasEntry {
                    as_: p.as_,
                    storage: p.storage,
                    address,
                },
            );
            self.scratch_graveyard.push(p.scratch);
        }
        self.generation += 1;
        info!(
            "BLAS 构建命令已录: {count} 条（一次 cmd_build_acceleration_structures，逐条独立 scratch），缓存 {} 条",
            self.entries.len()
        );
        count
    }

    /// 排销毁（资产退场/换内容）：AS 与 storage 进停尸房，等 `drain` 的帧侧等待后拆。
    pub(crate) fn retire(&mut self, id: AssetId<Mesh>) -> bool {
        let Some(entry) = self.entries.remove(&id) else {
            return false;
        };
        self.retired.push((entry.as_, entry.storage));
        self.generation += 1;
        true
    }

    /// 停尸房排空：先等全部在飞帧（GPU 用完旧 AS/scratch），再拆 AS/buffer。
    /// scratch 不需要等上传票据（它只被本进程的构建命令读）。
    ///
    /// # Errors
    /// `wait` 失败（停尸房保留，下次重试）。
    pub(crate) fn drain(&mut self, wait: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if self.retired.is_empty() && self.scratch_graveyard.is_empty() {
            return Ok(());
        }
        wait()?;
        let retired = std::mem::take(&mut self.retired);
        let retired_count = retired.len();
        for (as_, storage) in retired {
            // 先销毁 AS 再释放它引用的 buffer（storage 随作用域 Drop）
            unsafe { self.accel.destroy_acceleration_structure(as_, None) };
            drop(storage);
        }
        let scratch_count = self.scratch_graveyard.len();
        self.scratch_graveyard.clear();
        if retired_count > 0 || scratch_count > 0 {
            info!(
                "BLAS 停尸房排空:拆 AS/storage {retired_count} 组，释放 scratch {scratch_count} 条（帧侧等待已过）"
            );
        }
        Ok(())
    }

    /// 已建成 BLAS 的设备地址（待建/未建返回 None——地址表写 0，实例引用 0 合法）。
    #[must_use]
    pub(crate) fn address(&self, id: AssetId<Mesh>) -> Option<vk::DeviceAddress> {
        self.entries.get(&id).map(|e| e.address)
    }

    /// 建成/销毁计数：地址表按它判断要不要整表重写。
    #[must_use]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub(crate) fn count(&self) -> usize {
        self.entries.len()
    }

    /// 当前驻留 BLAS 的 storage 字节（AS 本体，不含 scratch）。
    #[must_use]
    pub(crate) fn bytes(&self) -> u64 {
        self.entries
            .values()
            .map(|e| e.storage.allocation_size())
            .sum()
    }

    /// 待建条数（BRP 读数用）。
    #[must_use]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for BlasRegistry {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle
        for (as_, _) in self.retired.drain(..) {
            unsafe { self.accel.destroy_acceleration_structure(as_, None) };
        }
        for p in self.pending.drain(..) {
            unsafe { self.accel.destroy_acceleration_structure(p.as_, None) };
        }
        for (_, entry) in self.entries.drain() {
            unsafe { self.accel.destroy_acceleration_structure(entry.as_, None) };
        }
    }
}
