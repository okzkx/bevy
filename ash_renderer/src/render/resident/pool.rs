//! 资产大池:顶点/索引两条
//! DEVICE_LOCAL 池 + bump 偏移分配 + 按资产身份去重的驻留缓存。
//!
//! 内存类型/usage/绑定/对齐的规则全部引用 [`crate::render::core::device::memory`] 的契约
//! ([`BufferRole::DevicePool`] 定案 + [`align_up`]),本模块只做三件事:
//!
//! 1. **bump 分配**:`used` 游标单调推进,顶点区间按 [`VERTEX_STRIDE`](32B)对齐、
//!    索引区间按 4B(U32)对齐——前者让每个顶点槽恰好一整条 stride,后者满足
//!    `vkCmdCopyBuffer` 偏移 4 倍数条款(VUID-00114)与 `vkCmdBindIndexBuffer`
//!    偏移按索引类型大小对齐条款(VUID-08783)。
//! 2. **驻留缓存**:`HashMap<AssetId<Mesh>, MeshSlot>` 按资产身份去重——已驻留
//!    资源不因每帧快照重复上传。
//!    槽位同时记录画侧要的引脚:`vertex_base`(vertexOffset =
//!    字节偏移 ÷ 32)、`first_index`(字节偏移 ÷ 4)与完成票据。
//! 3. **容量维护**:放不下时走显式维护路径——**不写越界**:先按
//!    "需量与现有驻留加余量"算新容量;等待全部在飞上传票据完成(迁移拷贝的
//!    源不再被 GPU 读)→ 分配新池 → 已驻留内容整段迁移(old→new 设备拷贝)
//!    → 等迁移票据 → **等图形侧最后使用**(`wait_draws` 回调:
//!    等全部帧槽 fence ⇒ 无在飞 draw 再读旧池)→ 才销毁旧池。迁移与销毁的
//!    等待全部记账,与正常上传分账(维护等待单列,
//!    不冒充"正常上传零 idle")。
//!
//! 销毁纪律:本类型 Drop 不等 GPU——"最后一次使用完成"由两个上游负责:在飞
//! 上传票据由容量维护在迁移前等待,进程级退出排空由 `host::teardown_vulkan` 的
//! `device_wait_idle` 先于一切资源 Drop 完成。
//!
//! 回收与复用:bump 游标之外立两条回收账——**停尸房**
//! ([`Self::retire`]:卸载/替换换下的旧区间,在飞 draw 可能还在读,排空前
//! 不得复用)与 **free list**([`Self::drain_graveyard`]:等"图形最后使用"
//! 后归还的空洞,[`Self::alloc`] first-fit 优先复用)。排空点与贴图停尸房
//! 同一节拍(flush 开头,`wait_draws` 收口),三条销毁路径(网格区间/旧图/
//! 描述符槽)共用同一次等待——责任边界契约②的回收半边统一落点。碎片不合并
//! (区间长度天然对齐倍数,first-fit 够用;合并/Slab 升级留显存压力出现再议)。

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
use super::uploader::{CopyRegion, UploadBatch, Uploader};
use crate::render::VERTEX_STRIDE;

/// 池内一段已分配区间(字节)。
#[derive(Clone, Copy, Debug)]
pub struct PoolRange {
    pub offset: u64,
    pub size: u64,
}

/// 一个已驻留网格的账本行:池内区间 + 绘制引脚 + 完成票据。
#[derive(Clone, Copy, Debug)]
pub struct MeshSlot {
    /// 顶点区间(顶点池内,offset 按 32B 对齐)。
    pub vertex: PoolRange,
    /// 索引区间(索引池内,offset 按 4B 对齐)。
    pub index: PoolRange,
    pub vertex_count: u32,
    pub index_count: u32,
    /// 局部包围盒(min,max;转换器逐点扫出)——对象表记录的包围体素材。
    pub local_aabb: ([f32; 3], [f32; 3]),
    /// 发布本内容的上传批次完成票据(图形提交按它等数据;CPU 侧仅在
    /// 复用/维护/退出时等待)。0 = 初始分配批(无拷贝)。
    pub ticket: u64,
}

impl MeshSlot {
    /// `vkCmdBindVertexBuffers` 用的首顶点号(vertexOffset)= 字节偏移 ÷ 32。
    #[must_use]
    pub fn vertex_base(&self) -> u32 {
        (self.vertex.offset / VERTEX_STRIDE) as u32
    }

    /// `vkCmdDrawIndexed` 的 firstIndex = 字节偏移 ÷ 4(U32)。
    #[must_use]
    pub fn first_index(&self) -> u32 {
        (self.index.offset / 4) as u32
    }
}

/// 维护分账统计(正常上传不进这里):扩容次数与累计停顿时长。
#[derive(Default, Clone, Copy)]
pub struct MaintenanceLedger {
    /// 发生过几次显式扩容(含迁移)。
    pub grow_count: u32,
    /// 扩容路径里"等待在飞完成 + 迁移"的累计停顿(毫秒)。
    pub pause_ms_total: u128,
}

/// 回收账本:卸载/替换换下区间的累计退休量 + 停尸房/free list 当前态。
/// 判定线"反复增删存量回基线"的读数口——resident 字节数随卸载回落,used 游标
/// 是历史峰值只增不减(复用不推进游标),两者分列不混报。
#[derive(Default, Clone, Copy)]
pub struct RecycleLedger {
    /// 卸载出账的驻留条目累计(含替换换下的旧条目)。
    pub retired_slots_total: u64,
    /// 退休区间字节累计(顶点/索引分列)。
    pub retired_vertex_bytes_total: u64,
    pub retired_index_bytes_total: u64,
    /// 当前在房区间组数(排空判定读数)。
    pub graveyard_groups: usize,
    /// 当前可复用空洞数与字节(复用账面的"池内闲置"口径)。
    pub free_ranges: usize,
    pub free_vertex_bytes: u64,
    pub free_index_bytes: u64,
    /// 当前驻留条目实占字节(判定线"存量回基线"的主读数)。
    pub resident_vertex_bytes: u64,
    pub resident_index_bytes: u64,
}

/// 顶点/索引 DEVICE_LOCAL 大池 + 资产身份驻留缓存。
#[derive(bevy::prelude::Resource)]
pub struct MeshPool {
    device: Device,
    contract: MemoryContract,
    /// 池 buffer 的共享族集合(定案:有专用 transfer 族时 = [transfer, graphics]
    /// 两族 CONCURRENT,所有权乒乓不值得;同族设备 = 空 = EXCLUSIVE。取数见
    /// `GpuBuffer::create_with_families` 的定案注释)。
    sharing_families: Vec<u32>,
    vertex: Option<GpuBuffer>,
    index: Option<GpuBuffer>,
    capacity_vertex: u64,
    capacity_index: u64,
    used_vertex: u64,
    used_index: u64,
    residency: HashMap<AssetId<Mesh>, MeshSlot>,
    /// 回收停尸房:卸载/替换换下的旧区间(顶点,索引),在飞 draw 可能
    /// 还在读——排空(等"图形最后使用")前不得复用,防覆盖判定线违反。
    graveyard: Vec<(PoolRange, PoolRange, u64)>,
    /// 回收就绪空洞:停尸房排空后归还的区间,alloc first-fit 优先
    /// 复用;偏移/长度保持对齐倍数(进入前已对齐,分裂按对齐倍数切)。
    free_vertex: Vec<PoolRange>,
    free_index: Vec<PoolRange>,
    ledger: MaintenanceLedger,
    recycled: RecycleLedger,
}

impl MeshPool {
    /// 空池:分配推迟到第一次 [`Self::ensure_capacity`](懒建,容量按当时的
    /// 需求算,不预付猜测的容量)。`sharing_families`:≥2 个不同族时池按
    /// CONCURRENT 创建(transfer 写 + graphics 读的异族设备),同族设备传空。
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

    /// 顶点池 buffer 句柄(未分配过则 None)。
    #[must_use]
    pub fn vertex_buffer(&self) -> Option<vk::Buffer> {
        self.vertex.as_ref().map(GpuBuffer::buffer)
    }

    /// 索引池 buffer 句柄(未分配过则 None)。
    #[must_use]
    pub fn index_buffer(&self) -> Option<vk::Buffer> {
        self.index.as_ref().map(GpuBuffer::buffer)
    }

    /// 资产是否已驻留(去重判定的唯一入口)。
    #[must_use]
    pub fn resident(&self, id: AssetId<Mesh>) -> Option<&MeshSlot> {
        self.residency.get(&id)
    }

    /// 已驻留网格数(验收统计口)。
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.residency.len()
    }

    /// 维护分账(验收统计口)。
    #[must_use]
    pub fn maintenance_ledger(&self) -> MaintenanceLedger {
        self.ledger
    }

    /// 两池当前用量/容量(验收统计口;used 是历史峰值,复用不推进游标)。
    #[must_use]
    pub fn usage(&self) -> ((u64, u64), (u64, u64)) {
        (
            (self.used_vertex, self.capacity_vertex),
            (self.used_index, self.capacity_index),
        )
    }

    /// 回收账本(读数口;驻留字节现算 O(条目数),调用频率=读数与日志)。
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

    /// 保证"现有驻留 + 新增"放得下:够用直接返回;不够则走扩容路径
    /// (初次懒建或显式维护迁移)。任何路径失败报错,调用方不得在未保证
    /// 容量的情况下 [`Self::alloc`]——那才可能写越界。
    ///
    /// `wait_draws`:销毁旧池前的"图形最后使用"等待(责任边界契约②的
    /// 图形半边)——调用方经它等全部帧槽 fence,保证没有在飞 draw 还在读旧池。
    /// 池不认识帧资源,以回调接线保持依赖单向(pool 不依赖 frames)。
    ///
    /// # Errors
    /// 新池分配/迁移拷贝提交失败,或等待在飞票据/帧侧等待失败。
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
        // 旧池总是成对存在(只在本函数尾部成对换上)
        let old_pools = self.vertex_buffer().zip(self.index_buffer());
        let migrating = old_pools.is_some();
        let pause = Instant::now();
        // 迁移的源(旧池)可能正被上一批上传在写:维护路径的第一步就是等全部
        // 在飞票据——这是"维护停顿"的本体,计入分账,不冒充正常上传零 idle。
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
            // 已驻留内容整段迁移:bump 布局保证 [0, used) 就是全部有效数据。
            // 迁移批自己的票据在销毁旧池前等完——覆盖上传侧;图形侧在下面
            // wait_draws 里收口
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
                .ok_or_else(|| VulkanError::Upload("内部矛盾:迁移批含拷贝却未产生提交".into()))?;
            uploader.wait_until(ticket)?;
            // 图形最后使用:等全部帧槽 fence——没有命令缓冲还在 GPU 上执行,
            // 在飞 draw 对旧池的读取必然已收口(契约②的双条件至此补齐)
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
        // 迁移路径在上方已等完迁移票据与帧侧等待;此刻旧池再无 GPU 引用,
        // 放心销毁。新池换上后 bump 游标保持不变(数据原偏移平移)。
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

    /// 扩容容量策略:需量 ×4/3 与现有 ×2 取大,再对齐 1MiB——"收齐的静态需求
    /// 加余量",不是逐资产精确贴合。
    fn next_capacity(needed: u64, current: u64) -> u64 {
        const MIB: u64 = 1024 * 1024;
        let target = needed
            .saturating_add(needed / 3)
            .max(current.saturating_mul(2))
            .max(MIB);
        align_up(target, MIB)
    }

    /// bump 划出一段区间(容量由 [`Self::ensure_capacity`] 先行保证;调用方
    /// 必须先 ensure 再 alloc,否则 debug_assert(发布构建下按纪律也不越界:
    /// 分配只在 ensure 之后发生是编排层的顺序契约)。alloc 优先复用 free
    /// list 空洞(first-fit,整段用或对齐倍数切分),查无才从 bump 游标划新
    /// ——free 区间必在 `[0, used)` 内且容量保证按 used+需量核算,复用天然
    /// 越界安全。
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

    /// first-fit 取一段区间:free list 首个放得下的空洞(整段用或切头留尾,
    /// 余量回表);查无才 bump 划新。`size` 须为区间对齐的倍数(顶点 32B 条,
    /// 索引 4B——转换器产出的字节长度天然满足),余量偏移才保持对齐。
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

    /// 卸载驻留条目:区间进停尸房(**不复用**——在飞 draw 可能还在读
    /// 旧内容,排空后才归还 free list),返回释放的字节 `(顶点, 索引)`;未驻留
    /// = None 幂等(重复 Removed/无驻留身份不误动作)。上传票据随行
    /// (排空双条件之一)。
    pub fn retire(&mut self, id: AssetId<Mesh>) -> Option<(u64, u64)> {
        let slot = self.residency.remove(&id)?;
        self.graveyard.push((slot.vertex, slot.index, slot.ticket));
        self.recycled.retired_slots_total += 1;
        self.recycled.retired_vertex_bytes_total += slot.vertex.size;
        self.recycled.retired_index_bytes_total += slot.index.size;
        Some((slot.vertex.size, slot.index.size))
    }

    /// 排空回收停尸房:双条件齐备后旧区间归还 free
    /// list 可复用——`wait_draws` 等"图形最后使用"(帧槽 fence),`uploader`
    /// 等"上传完成"(停尸房票据的时间线信号量;最小化期间 flush 仍可提交
    /// transfer 而无图形提交,帧 fence 完成不蕴含上传完成)。空房直接返回不
    /// 等待(稳态零成本);失败保留停尸房下帧重试(区间多活一帧,无悬空)。池
    /// 迁移([`Self::ensure_capacity`])与排空的先后由编排层保证:ensure 先等
    /// 帧侧,排空在 flush 开头先于 ensure——迁移时停尸房必空,区间数据
    /// 随 `[0, used)` 整段平移,偏移不变语义不变。
    ///
    /// # Errors
    /// 帧侧等待或票据等待失败(排空未发生,区间保留下帧重试)。
    pub fn drain_graveyard(
        &mut self,
        uploader: &Uploader,
        wait_draws: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        // 票据取最大值一次等待:同一条时间线信号量按值单调,等最大即等全部
        let max_ticket = self
            .graveyard
            .iter()
            .map(|(_, _, ticket)| *ticket)
            .fold(0u64, u64::max);
        uploader.wait_until(max_ticket)?;
        let count = self.graveyard.len();
        for (vertex, index, _) in self.graveyard.drain(..) {
            self.free_vertex.push(vertex);
            self.free_index.push(index);
        }
        info!(
            "网格回收停尸房排空:归还 {count} 组区间(双条件齐备:图形最后使用 + 上传票据)"
        );
        Ok(())
    }

    /// 登记驻留:提交成功拿到票据后调用;同身份重复登记是编排层 bug,报错。
    ///
    /// # Errors
    /// 该资产身份已有驻留条目(去重纪律被打破)。
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
            return Err(VulkanError::Upload(format!(
                "驻留缓存重复登记同身份资产 {id:?}——上传编排的去重判定被打破"
            )));
        }
        Ok(())
    }

    /// 同身份内容替换登记:条目已在 = 换新槽位、返回旧槽位。旧区间进
    /// 停尸房——在飞 draw 可能还在读旧内容,排空(等"图形最后使用")
    /// 后归还 free list 可复用。与
    /// [`Self::commit`] 的分工:编排层持失效凭据(AssetInvalidations)才允许
    /// 走 replace,纪律边界在编排层,本层只提供换条目的能力。
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "驻留条目即七项事实(身份/两池区间/两计数/包围盒/票据),与 commit 同款"
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
            self.graveyard.push((old.vertex, old.index, old.ticket));
            self.recycled.retired_slots_total += 1;
            self.recycled.retired_vertex_bytes_total += old.vertex.size;
            self.recycled.retired_index_bytes_total += old.index.size;
        }
        old
    }
}

impl Drop for MeshPool {
    fn drop(&mut self) {
        // 契约边界(与 GpuBuffer::Drop 同款):不等 GPU。在飞上传票据与图形侧
        // 最后使用由容量维护在销毁旧池前等待(双条件);退出排空由
        // teardown_vulkan 的 device_wait_idle 先于一切 Drop 完成。
        self.vertex = None;
        self.index = None;
    }
}
