//! 网格大池：顶点/索引两条 DEVICE_LOCAL 池 + bump 偏移分配 + 按资产身份去重的驻留缓存。
//!
//! 内存类型/usage/对齐规则取自 [`crate::render::core::device::memory`]（[`BufferRole::DevicePool`]
//! + [`align_up`]），本模块只做三件事：
//!
//! 1. **bump 分配**：`used` 游标单调推进。顶点区间按 [`VERTEX_STRIDE`]（32B）对齐，让每个
//!    顶点恰好占一整条 stride；索引区间按 4B（U32）对齐，满足 `vkCmdCopyBuffer` 偏移 4 倍数
//!    （VUID-00114）与 `vkCmdBindIndexBuffer` 偏移按索引类型对齐（VUID-08783）。
//! 2. **驻留缓存**：`HashMap<AssetId<Mesh>, MeshSlot>` 按资产身份去重，已驻留的网格不重复上传。
//! 3. **扩容迁移**：放不下时不写越界，走显式扩容：算新容量 → 等全部在飞上传（迁移源不再被写）
//!    → 建新池 → 已驻留内容整段拷到新池 → 等迁移完成 → 等全部帧槽 fence（`wait_draws`，没有
//!    在飞 draw 再读旧池）→ 销毁旧池。扩容停顿单独记账（[`MaintenanceLedger`]），不算进正常上传。
//!
//! 回收：卸载/替换换下的旧区间先进待回收账（[`MeshPool::retire`]，在飞 draw 可能还在读），
//! [`MeshPool::drain_graveyard`] 等 GPU 用完后归还空闲表，[`MeshPool::alloc`] first-fit 优先复用。
//! 与贴图、描述符槽的回收共用 flush 开头的同一次等待。空洞不合并（区间长度天然是对齐倍数，
//! first-fit 够用）。
//!
//! Drop 不等 GPU：在飞上传由扩容路径在迁移前等完，退出时 `host::teardown_vulkan` 已先
//! `device_wait_idle`。

use std::collections::HashMap;
use std::time::Instant;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::log::info;
use bevy::mesh::Mesh;

use crate::common::error::{Result, VulkanError};

use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::{align_up, MemoryContract};
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::device::transfer::{CopyRegion, UploadBatch, Uploader};
use crate::render::VERTEX_STRIDE;

/// 池内一段已分配区间(字节)。
#[derive(Clone, Copy, Debug)]
pub struct PoolRange {
    pub offset: u64,
    pub size: u64,
}

/// 一个已驻留网格的账本行：池内区间 + 计数 + 包围盒 + 上传票据。
#[derive(Clone, Copy, Debug)]
pub struct MeshSlot {
    /// 顶点区间（offset 按 32B 对齐）。
    pub vertex: PoolRange,
    /// 索引区间（offset 按 4B 对齐）。
    pub index: PoolRange,
    pub vertex_count: u32,
    pub index_count: u32,
    /// 局部包围盒（min, max；转换时逐点扫出），写进对象表记录。
    pub local_aabb: ([f32; 3], [f32; 3]),
    /// 发布本内容的上传批次票据：图形提交在 GPU 侧等它；CPU 只在回收/扩容/退出时等。
    /// 0 = 初始分配批（无拷贝）。
    pub ticket: u64,
}

impl MeshSlot {
    /// `vkCmdDrawIndexed` 的 vertexOffset = 字节偏移 ÷ 32。
    #[must_use]
    pub fn vertex_base(&self) -> u32 {
        (self.vertex.offset / VERTEX_STRIDE) as u32
    }

    /// `vkCmdDrawIndexed` 的 firstIndex = 字节偏移 ÷ 4（U32）。
    #[must_use]
    pub fn first_index(&self) -> u32 {
        (self.index.offset / 4) as u32
    }
}

/// 扩容统计（正常上传不计入）：扩容次数与累计停顿。
#[derive(Default, Clone, Copy)]
pub struct MaintenanceLedger {
    /// 显式扩容（含迁移）次数。
    pub grow_count: u32,
    /// 扩容路径里"等在飞上传 + 迁移 + 等帧槽 fence"的累计停顿（毫秒）。
    pub pause_ms_total: u128,
}

/// 回收账：累计退场量 + 待回收/空闲区间的当前状态。驻留字节随卸载回落，而 used 游标是
/// 历史峰值只增不减（复用不推进），两者分开报。
#[derive(Default, Clone, Copy)]
pub struct RecycleLedger {
    /// 累计退场的驻留条目数（含替换换下的旧条目）。
    pub retired_slots_total: u64,
    /// 累计退场的区间字节（顶点/索引分列）。
    pub retired_vertex_bytes_total: u64,
    pub retired_index_bytes_total: u64,
    /// 当前待回收的区间组数。
    pub graveyard_groups: usize,
    /// 当前可复用的空洞数与字节。
    pub free_ranges: usize,
    pub free_vertex_bytes: u64,
    pub free_index_bytes: u64,
    /// 当前驻留条目实占字节。
    pub resident_vertex_bytes: u64,
    pub resident_index_bytes: u64,
}

/// 顶点/索引 DEVICE_LOCAL 大池 + 资产身份驻留缓存。
#[derive(bevy::prelude::Resource)]
pub struct MeshPool {
    device: Device,
    contract: MemoryContract,
    /// 池 buffer 的共享族：有专用 transfer 族时为 [transfer, graphics]（CONCURRENT，不做
    /// 所有权转移）；同族设备为空（EXCLUSIVE）。
    sharing_families: Vec<u32>,
    vertex: Option<GpuBuffer>,
    index: Option<GpuBuffer>,
    capacity_vertex: u64,
    capacity_index: u64,
    used_vertex: u64,
    used_index: u64,
    residency: HashMap<AssetId<Mesh>, MeshSlot>,
    /// 待回收的旧区间（顶点, 索引, 票据）：在飞 draw 可能还在读，排空前不得复用。
    graveyard: Vec<(PoolRange, PoolRange, u64)>,
    /// 已回收可复用的空洞，alloc first-fit 优先取；偏移/长度保持对齐倍数。
    free_vertex: Vec<PoolRange>,
    free_index: Vec<PoolRange>,
    ledger: MaintenanceLedger,
    recycled: RecycleLedger,
}

impl MeshPool {
    /// 空池：第一次 [`Self::ensure_capacity`] 才按当时的需求分配。`sharing_families`
    /// 有 ≥2 个不同族时池按 CONCURRENT 创建，同族设备传空。
    pub fn new(device: &Device, contract: &MemoryContract, sharing_families: &[u32]) -> Self {
        Self {
            device: device.clone(),
            contract: contract.clone(),
            sharing_families: sharing_families.to_vec(),
            vertex: None,
            index: None,
            capacity_vertex: 0,
            capacity_index: 0,
            used_vertex: 0,
            used_index: 0,
            residency: HashMap::new(),
            graveyard: Vec::new(),
            free_vertex: Vec::new(),
            free_index: Vec::new(),
            ledger: MaintenanceLedger::default(),
            recycled: RecycleLedger::default(),
        }
    }

    /// 顶点池 buffer 句柄（还没分配过则 None）。
    #[must_use]
    pub fn vertex_buffer(&self) -> Option<vk::Buffer> {
        self.vertex.as_ref().map(GpuBuffer::buffer)
    }

    /// 索引池 buffer 句柄（还没分配过则 None）。
    #[must_use]
    pub fn index_buffer(&self) -> Option<vk::Buffer> {
        self.index.as_ref().map(GpuBuffer::buffer)
    }

    /// 已驻留则返回其账本行（去重判定的唯一入口）。
    #[must_use]
    pub fn resident(&self, id: AssetId<Mesh>) -> Option<&MeshSlot> {
        self.residency.get(&id)
    }

    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.residency.len()
    }

    #[must_use]
    pub fn maintenance_ledger(&self) -> MaintenanceLedger {
        self.ledger
    }

    /// 两池 ((顶点 used, 容量), (索引 used, 容量))；used 是历史峰值，复用不推进。
    #[must_use]
    pub fn usage(&self) -> ((u64, u64), (u64, u64)) {
        (
            (self.used_vertex, self.capacity_vertex),
            (self.used_index, self.capacity_index),
        )
    }

    /// 回收账（驻留字节每次现算，O(条目数)；只在读数与日志时调用）。
    #[must_use]
    pub fn recycle_ledger(&self) -> RecycleLedger {
        let mut r = self.recycled;
        r.graveyard_groups = self.graveyard.len();
        r.free_ranges = self.free_vertex.len() + self.free_index.len();
        r.free_vertex_bytes = self.free_vertex.iter().map(|x| x.size).sum();
        r.free_index_bytes = self.free_index.iter().map(|x| x.size).sum();
        let (v, i) = self
            .residency
            .values()
            .fold((0u64, 0u64), |(v, i), s| (v + s.vertex.size, i + s.index.size));
        r.resident_vertex_bytes = v;
        r.resident_index_bytes = i;
        r
    }

    /// 保证"已用 + 新增"放得下：够用直接返回，不够则首次建池或扩容迁移（步骤见模块注释）。
    /// 失败时调用方不得继续 [`Self::alloc`]，否则会写越界。
    ///
    /// `wait_draws`：销毁旧池前等全部帧槽 fence。用回调而不是直接拿帧资源，pool 就不依赖
    /// frame。
    ///
    /// # Errors
    /// 新池分配/迁移拷贝提交失败，或等待失败。
    pub fn ensure_capacity(
        &mut self,
        uploader: &mut Uploader,
        extra_vertex: u64,
        extra_index: u64,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let need_vertex = self.used_vertex + extra_vertex;
        let need_index = self.used_index + extra_index;
        if self.vertex.is_some()
            && need_vertex <= self.capacity_vertex
            && self.index.is_some()
            && need_index <= self.capacity_index
        {
            return Ok(());
        }
        let new_vertex_cap = Self::next_capacity(need_vertex, self.capacity_vertex);
        let new_index_cap = Self::next_capacity(need_index, self.capacity_index);
        // 旧池总是成对存在（只在本函数尾部成对换上）
        let old_pools = self.vertex_buffer().zip(self.index_buffer());
        let migrating = old_pools.is_some();
        let pause = Instant::now();
        // 迁移源（旧池）可能正被上一批上传在写，先等全部在飞上传
        if migrating {
            uploader.wait_all_uploads()?;
        }
        let new_vertex = GpuBuffer::create_with_families(
            &self.device,
            &self.contract,
            new_vertex_cap,
            BufferRole::DevicePool,
            &self.sharing_families,
        )?;
        let new_index = GpuBuffer::create_with_families(
            &self.device,
            &self.contract,
            new_index_cap,
            BufferRole::DevicePool,
            &self.sharing_families,
        )?;
        if let Some((old_vertex, old_index)) = old_pools {
            // 已驻留内容整段迁移：bump 布局保证 [0, used) 就是全部有效数据
            let ticket = uploader
                .submit_batch(UploadBatch {
                    device_copies: vec![
                        CopyRegion {
                            src: old_vertex,
                            dst: new_vertex.buffer(),
                            src_offset: 0,
                            dst_offset: 0,
                            size: self.used_vertex,
                        },
                        CopyRegion {
                            src: old_index,
                            dst: new_index.buffer(),
                            src_offset: 0,
                            dst_offset: 0,
                            size: self.used_index,
                        },
                    ],
                    ..Default::default()
                })?
                .ok_or_else(|| VulkanError::Contract("内部矛盾:迁移批含拷贝却未产生提交".into()))?;
            uploader.wait_until(ticket)?;
            // 等全部帧槽 fence：没有在飞 draw 还在读旧池，之后才能销毁旧池
            wait_draws()?;
        }
        let pause_ms = pause.elapsed().as_millis();
        if migrating {
            info!(
                "池扩容维护:顶点 {old_v}B→{new_v}B 索引 {old_i}B→{new_i}B,迁移 {mv}B+{mi}B(等待在飞上传+迁移+图形最后使用,停顿 {pause_ms}ms)",
                old_v = self.capacity_vertex,
                new_v = new_vertex_cap,
                old_i = self.capacity_index,
                new_i = new_index_cap,
                mv = self.used_vertex,
                mi = self.used_index,
            );
        } else {
            info!("池初建:顶点 {new_vertex_cap}B 索引 {new_index_cap}B(按当时收齐的需求 + 余量,懒分配不预付)");
        }
        // 换上新池即销毁旧池（迁移路径上方已等完）；bump 游标不变，数据偏移原样平移
        self.vertex = Some(new_vertex);
        self.index = Some(new_index);
        self.capacity_vertex = new_vertex_cap;
        self.capacity_index = new_index_cap;
        if migrating {
            self.ledger.grow_count += 1;
            self.ledger.pause_ms_total += pause_ms;
        }
        Ok(())
    }

    /// 扩容容量：需量 ×4/3 与现有 ×2 取大（至少 1MiB），再对齐 1MiB。
    fn next_capacity(needed: u64, current: u64) -> u64 {
        const MIB: u64 = 1024 * 1024;
        let target = needed
            .saturating_add(needed / 3)
            .max(current.saturating_mul(2))
            .max(MIB);
        align_up(target, MIB)
    }

    /// 分一段顶点区间和一段索引区间：先 first-fit 复用空洞，没有合适的才从 bump 游标划新。
    /// 调用方必须先 [`Self::ensure_capacity`]（debug 构建断言）；空洞都在 `[0, used)` 内，
    /// 复用不会越界。
    pub fn alloc(&mut self, vertex_size: u64, index_size: u64) -> (PoolRange, PoolRange) {
        let vertex = Self::take_range(&mut self.free_vertex, &mut self.used_vertex, vertex_size);
        let index = Self::take_range(&mut self.free_index, &mut self.used_index, index_size);
        debug_assert!(
            vertex.offset + vertex.size <= self.capacity_vertex
                && index.offset + index.size <= self.capacity_index,
            "alloc 前必须先 ensure_capacity(bump 不做静默扩容)"
        );
        (vertex, index)
    }

    /// first-fit 取一段区间：首个放得下的空洞整段用或切头、余量放回；没有才 bump 划新。
    /// `size` 须是对齐的倍数（顶点 32B、索引 4B，转换产物天然满足），余量偏移才保持对齐。
    fn take_range(free: &mut Vec<PoolRange>, used: &mut u64, size: u64) -> PoolRange {
        debug_assert!(
            size > 0,
            "零长区间不进池(转换产物必有内容)"
        );
        if let Some(pos) = free.iter().position(|r| r.size >= size) {
            let r = free.remove(pos);
            if r.size > size {
                free.push(PoolRange {
                    offset: r.offset + size,
                    size: r.size - size,
                });
            }
            return PoolRange {
                offset: r.offset,
                size,
            };
        }
        let offset = *used;
        *used = offset + size;
        PoolRange { offset, size }
    }

    /// 卸载驻留条目：区间进待回收账（在飞 draw 可能还在读，排空后才可复用），返回释放的
    /// 字节 `(顶点, 索引)`；未驻留返回 None（幂等）。
    pub fn retire(&mut self, id: AssetId<Mesh>) -> Option<(u64, u64)> {
        let slot = self.residency.remove(&id)?;
        self.bury(&slot);
        Some((slot.vertex.size, slot.index.size))
    }

    /// 把一个换下的条目记入待回收账与累计账。
    fn bury(&mut self, slot: &MeshSlot) {
        self.graveyard.push((slot.vertex, slot.index, slot.ticket));
        self.recycled.retired_slots_total += 1;
        self.recycled.retired_vertex_bytes_total += slot.vertex.size;
        self.recycled.retired_index_bytes_total += slot.index.size;
    }

    /// 排空待回收账：两个条件都满足才归还空闲表——`wait_draws` 等全部帧槽 fence（没有在飞
    /// draw），`uploader` 等这些区间的上传票据完成。没有待回收区间时不等待。
    ///
    /// 坑：帧 fence 不代表上传完成——窗口最小化时 flush 仍会提交 transfer 而没有图形提交，
    /// 所以票据必须单独等。
    ///
    /// 与扩容的顺序由调用方保证：排空在 flush 开头、先于 ensure_capacity，所以迁移时待回收
    /// 账一定是空的。
    ///
    /// # Errors
    /// 等待失败（待回收账保留，下帧重试）。
    pub fn drain_graveyard(
        &mut self,
        uploader: &Uploader,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        // 同一条时间线信号量按值单调，等最大票据即等全部
        let max_ticket = self.graveyard.iter().map(|(_, _, ticket)| *ticket).max().unwrap_or(0);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        for (vertex, index, _) in self.graveyard.drain(..) {
            self.free_vertex.push(vertex);
            self.free_index.push(index);
        }
        info!(
            "网格区间待回收队列排空:归还 {count} 组区间(双条件齐备:图形最后使用 + 上传票据)"
        );
        Ok(())
    }

    /// 登记驻留：批次提交成功拿到票据后调用。
    ///
    /// # Errors
    /// 该资产身份已有驻留条目（调用方去重出错）。
    #[expect(
        clippy::too_many_arguments,
        reason = "驻留条目即七项事实(身份/两池区间/两计数/包围盒/票据),拆结构体的收益不抵两处解构"
    )]
    pub fn commit(
        &mut self,
        id: AssetId<Mesh>,
        vertex: PoolRange,
        index: PoolRange,
        vertex_count: u32,
        index_count: u32,
        local_aabb: ([f32; 3], [f32; 3]),
        ticket: u64,
    ) -> Result<()> {
        let slot = MeshSlot {
            vertex,
            index,
            vertex_count,
            index_count,
            local_aabb,
            ticket,
        };
        if self.residency.insert(id, slot).is_some() {
            return Err(VulkanError::Contract(format!(
                "驻留缓存重复登记同身份资产 {id:?}——上传编排的去重判定被打破"
            )));
        }
        Ok(())
    }

    /// 同身份内容替换：换上新条目并返回旧条目，旧区间进待回收账。只有持失效凭据
    /// （AssetInvalidations）的调用方才应走这里，判断归调用方。
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "驻留条目由七项组成(身份/两池区间/两计数/包围盒/票据),与 commit 参数一致"
    )]
    pub fn replace(
        &mut self,
        id: AssetId<Mesh>,
        vertex: PoolRange,
        index: PoolRange,
        vertex_count: u32,
        index_count: u32,
        local_aabb: ([f32; 3], [f32; 3]),
        ticket: u64,
    ) -> Option<MeshSlot> {
        let slot = MeshSlot {
            vertex,
            index,
            vertex_count,
            index_count,
            local_aabb,
            ticket,
        };
        let old = self.residency.insert(id, slot);
        if let Some(old) = &old {
            self.bury(old);
        }
        old
    }
}
