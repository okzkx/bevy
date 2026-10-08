//! 资产大池(3.2.2.2 池与缓存 + 3.2.2.3 容量与销毁):顶点/索引两条
//! DEVICE_LOCAL 池 + bump 偏移分配 + 按资产身份去重的驻留缓存。
//!
//! 内存类型/usage/绑定/对齐的规则全部引用 [`super::resources`] 的契约
//! ([`BufferRole::DevicePool`] 定案 + [`align_up`]),本模块只做三件事:
//!
//! 1. **bump 分配**:`used` 游标单调推进,顶点区间按 [`VERTEX_STRIDE`](32B)对齐、
//!    索引区间按 4B(U32)对齐——前者让每个顶点槽恰好一整条 stride,后者满足
//!    `vkCmdCopyBuffer` 偏移 4 倍数条款(VUID-00114)与 `vkCmdBindIndexBuffer`
//!    偏移按索引类型大小对齐条款(VUID-08783)。不回收空洞:静态场景只增不删,
//!    回收/淘汰是步骤 4 之后的账本问题(施工计划 §4 本步不做)。
//! 2. **驻留缓存**:`HashMap<AssetId<Mesh>, MeshSlot>` 按资产身份去重——同一柄
//!    不因每帧快照重复上传(施工计划 §2"已驻留资源不因每帧快照而重复上传")。
//!    槽位同时记录 3.4 画侧要的引脚:`vertex_base`(vertexOffset =
//!    字节偏移 ÷ 32)、`first_index`(字节偏移 ÷ 4)与完成票据。
//! 3. **容量维护**(3.2.2.3):放不下时走显式维护路径——**不写越界**:先按
//!    "需量与现有驻留加余量"算新容量;等待全部在飞上传票据完成(迁移拷贝的
//!    源不再被 GPU 读)→ 分配新池 → 已驻留内容整段迁移(old→new 设备拷贝)
//!    → 等迁移票据 → 才销毁旧池。迁移与销毁的等待全部记账,与正常上传分账
//!    (施工计划 §0 判定线 6:维护等待单列,不冒充"正常上传零 idle")。
//!    图形侧最后使用:当前 M2 帧循环只清屏,图形队列尚未消费池数据——该等待
//!    面在 3.4 接入 draw 时补齐(边界冻结见责任边界文档)。
//!
//! 销毁纪律:本类型 Drop 不等 GPU——"最后一次使用完成"由两个上游负责:在飞
//! 上传票据由容量维护在迁移前等待,进程级退出排空由 `host::teardown_vulkan` 的
//! `device_wait_idle` 先于一切资源 Drop 完成(D4 定案)。M2 无资产淘汰,驻留
//! 缓存条目与池同寿命;资产卸载/重载的失效语义留步骤 4。

use std::collections::HashMap;
use std::time::Instant;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::log::info;
use bevy::mesh::Mesh;

use crate::error::VulkanError;

use super::resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
use super::uploader::{CopyRegion, UploadBatch, Uploader};
use super::VERTEX_STRIDE;

/// 池内一段已分配区间(字节)。
#[derive(Clone, Copy, Debug)]
pub struct PoolRange {
    pub offset: u64,
    pub size: u64,
}

/// 一个已驻留网格的账本行:池内区间 + 3.4 绘制引脚 + 完成票据。
#[derive(Clone, Copy, Debug)]
pub struct MeshSlot {
    /// 顶点区间(顶点池内,offset 按 32B 对齐)。
    pub vertex: PoolRange,
    /// 索引区间(索引池内,offset 按 4B 对齐)。
    pub index: PoolRange,
    pub vertex_count: u32,
    pub index_count: u32,
    /// 发布本内容的上传批次完成票据(3.4 图形提交按它等数据;CPU 侧仅在
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

/// 顶点/索引 DEVICE_LOCAL 大池 + 资产身份驻留缓存(3.2.2.2/3.2.2.3)。
#[derive(bevy::prelude::Resource)]
pub struct MeshPool {
    device: Device,
    contract: MemoryContract,
    vertex: Option<GpuBuffer>,
    index: Option<GpuBuffer>,
    capacity_vertex: u64,
    capacity_index: u64,
    used_vertex: u64,
    used_index: u64,
    residency: HashMap<AssetId<Mesh>, MeshSlot>,
    ledger: MaintenanceLedger,
}

impl MeshPool {
    /// 空池:分配推迟到第一次 [`Self::ensure_capacity`](懒建,容量按当时的
    /// 需求算,不预付猜测的容量)。
    pub fn new(device: &Device, contract: &MemoryContract) -> Self {
        Self {
            device: device.clone(),
            contract: contract.clone(),
            vertex: None,
            index: None,
            capacity_vertex: 0,
            capacity_index: 0,
            used_vertex: 0,
            used_index: 0,
            residency: HashMap::new(),
            ledger: MaintenanceLedger::default(),
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

    /// 两池当前用量/容量(验收统计口)。
    #[must_use]
    pub fn usage(&self) -> ((u64, u64), (u64, u64)) {
        (
            (self.used_vertex, self.capacity_vertex),
            (self.used_index, self.capacity_index),
        )
    }

    /// 保证"现有驻留 + 新增"放得下:够用直接返回;不够则走扩容路径
    /// (初次懒建或显式维护迁移)。任何路径失败报错,调用方不得在未保证
    /// 容量的情况下 [`Self::alloc`]——那才可能写越界。
    ///
    /// # Errors
    /// 新池分配/迁移拷贝提交失败,或等待在飞票据失败。
    pub fn ensure_capacity(
        &mut self,
        uploader: &mut Uploader,
        extra_vertex: u64,
        extra_index: u64,
    ) -> Result<(), VulkanError> {
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
        let migrating = self.vertex.is_some();
        let pause = Instant::now();
        // 迁移的源(旧池)可能正被上一批上传在写:维护路径的第一步就是等全部
        // 在飞票据——这是"维护停顿"的本体,计入分账,不冒充正常上传零 idle。
        if migrating {
            uploader.wait_all_uploads()?;
        }
        let new_vertex = GpuBuffer::create(
            &self.device,
            &self.contract,
            new_vertex_cap,
            BufferRole::DevicePool,
        )?;
        let new_index = GpuBuffer::create(
            &self.device,
            &self.contract,
            new_index_cap,
            BufferRole::DevicePool,
        )?;
        if migrating {
            // 已驻留内容整段迁移:bump 布局保证 [0, used) 就是全部有效数据。
            // 迁移批自己的票据在销毁旧池前等完(旧池最后使用 = 这次拷贝)
            let ticket = uploader
                .submit_batch(UploadBatch {
                    device_copies: vec![
                        CopyRegion {
                            src: self.vertex_buffer().expect("迁移时旧顶点池必在"),
                            dst: new_vertex.buffer(),
                            src_offset: 0,
                            dst_offset: 0,
                            size: self.used_vertex,
                        },
                        CopyRegion {
                            src: self.index_buffer().expect("迁移时旧索引池必在"),
                            dst: new_index.buffer(),
                            src_offset: 0,
                            dst_offset: 0,
                            size: self.used_index,
                        },
                    ],
                    ..Default::default()
                })?
                .expect("迁移批含拷贝,必有票据");
            uploader.wait_until(ticket)?;
        }
        let pause_ms = pause.elapsed().as_millis();
        if migrating {
            info!(
                "池扩容维护:顶点 {old_v}B→{new_v}B 索引 {old_i}B→{new_i}B,迁移 {mv}B+{mi}B(等待在飞 + 设备拷贝 + 等迁移票据,停顿 {pause_ms}ms)",
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
        // 迁移路径在 submit_migration 内已等迁移票据完成;此刻旧池再无 GPU
        // 引用,放心销毁。新池换上后 bump 游标保持不变(数据原偏移平移)。
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

    /// bump 划出一段区间(容量由 [`Self::ensure_capacity`])先行保证;调用方
    /// 必须先 ensure 再 alloc,否则 debug_assert(发布构建下按纪律也不越界:
    /// 分配只在 ensure 之后发生是编排层的顺序契约)。
    pub fn alloc(&mut self, vertex_size: u64, index_size: u64) -> (PoolRange, PoolRange) {
        let vertex_offset = align_up(self.used_vertex, VERTEX_STRIDE);
        let index_offset = align_up(self.used_index, 4);
        debug_assert!(
            vertex_offset + vertex_size <= self.capacity_vertex
                && index_offset + index_size <= self.capacity_index,
            "alloc 前必须先 ensure_capacity(bump 不做静默扩容)"
        );
        self.used_vertex = vertex_offset + vertex_size;
        self.used_index = index_offset + index_size;
        (
            PoolRange {
                offset: vertex_offset,
                size: vertex_size,
            },
            PoolRange {
                offset: index_offset,
                size: index_size,
            },
        )
    }

    /// 登记驻留:提交成功拿到票据后调用;同身份重复登记是编排层 bug,报错。
    ///
    /// # Errors
    /// 该资产身份已有驻留条目(去重纪律被打破)。
    pub fn commit(
        &mut self,
        id: AssetId<Mesh>,
        vertex: PoolRange,
        index: PoolRange,
        vertex_count: u32,
        index_count: u32,
        ticket: u64,
    ) -> Result<(), VulkanError> {
        let slot = MeshSlot {
            vertex,
            index,
            vertex_count,
            index_count,
            ticket,
        };
        if self.residency.insert(id, slot).is_some() {
            return Err(VulkanError::Upload(format!(
                "驻留缓存重复登记同身份资产 {id:?}——上传编排的去重判定被打破"
            )));
        }
        Ok(())
    }
}

impl Drop for MeshPool {
    fn drop(&mut self) {
        // 契约边界(与 GpuBuffer::Drop 同款):不等 GPU。在飞上传票据由容量
        // 维护在迁移前等待;图形侧最后使用与退出排空分别归 3.4 接线与
        // teardown_vulkan 的 device_wait_idle(D4)。
        self.vertex = None;
        self.index = None;
    }
}
