//! 光源常驻表：punctual 光源的常驻 storage 表——槽位 + 增量账本 + free list
//! （与对象表同款增量形态）。
//!
//! 三条设计钉：
//! - **帧槽双缓冲 + fence 纪律**：表本体 HOST_VISIBLE ×`MAX_FRAMES_IN_FLIGHT`
//!   帧槽，脏行增量重写发生在 `wait_for_slot` 之后（与模型环同一条 fence
//!   证据）——宿主写与 GPU 读不同时落在同一份物理内存上。稳态（灯无增删改）
//!   零上传；变更帧只写脏行，且同一脏行要写到两个帧槽都覆盖才出账（环切
//!   后另一槽还留着旧内容）。
//! - **槽位稳定性**：cluster 逐 tile 光源列表存的是表槽号（跨帧稳定身份），
//!   分配用 [`SlotAllocator`]；回收槽**立即清零**（radius=0 的死灯
//!   语义：cluster 求交与着色双侧按 `r > 0` 门卫跳过），停尸房不设——1 帧
//!   内旧列表引用已换内容的槽最坏产生一次视觉闪变，无悬空读（内容恒有效），
//!   灯量级（百以内）不值得对象表级 graveyard 机器。
//! - **方向光不进表**：方向光是全局量语义，走 set1 UBO 单光链（bevy GPU 同款）；
//!   表只装 punctual（点光，type 字段预留 spot）。
//!
//! shadow 标志位（flags bit0）本表只记账不消费，由阴影采样侧决定是否使用。

use ash::vk;
use bevy::log::info;

use crate::common::bytes::{put_u32, put_vec4};
use crate::common::error::{Result, VulkanError};
use super::slot_allocator::SlotAllocator;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::MemoryContract;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 光源表容量（槽位数；BRP 注入的测试灯与远期场景灯共用）。
pub(crate) const LIGHT_TABLE_CAPACITY: u32 = 256;

/// 光源记录字节数（vec4×2 + flags + 垫 = 64B，对齐 16；与 WGSL `LightRecord`
/// 互证。注意 WGSL 侧 `_pad: vec3<u32>` 的成员对齐把步长撑到 64（pad 落
/// @48）——Rust 侧 pack 按 64B 排布，36..48 区保持 0）。
pub(crate) const LIGHT_RECORD_SIZE: u64 = 64;

/// 光源类型（flags bits 8..15）：点光已实现；spot 只预留记录形状（锥体簇求交
/// 未实现）。
pub const LIGHT_TYPE_POINT: u32 = 0;
#[expect(dead_code, reason = "spot 未实现；记录形状预留（type 域契约）")]
pub const LIGHT_TYPE_SPOT: u32 = 1;

pub const LIGHT_FLAG_SHADOW: u32 = 1 << 0;
pub const LIGHT_TYPE_SHIFT: u32 = 8;

/// 一条光源记录（Rust 镜像；[`LightTable::spawn`] 入参与 census 参照共用）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightRecord {
    /// 世界空间位置（xyz）+ 作用半径（w，米；0 = 死灯，双侧门卫跳过）。
    pub pos_radius: [f32; 4],
    /// 颜色（rgb 线性）× 强度（w，candela 域；曝光换算归消费端）。
    pub color_intensity: [f32; 4],
    /// bit0 = shadow；bits 8..15 = type（[`LIGHT_TYPE_POINT`]）。
    pub flags: u32,
}

impl LightRecord {
    /// 死灯记录（回收槽清零语义：radius=0，双侧门卫跳过）。
    pub const DEAD: Self = Self {
        pos_radius: [0.0; 4],
        color_intensity: [0.0; 4],
        flags: 0,
    };

    /// 打包 64B 进表（与 WGSL `LightRecord` 同序互证：pos @0 / color @16 /
    /// flags @32 / WGSL 垫 @48——36..48 区保持 0，成员对齐由 64B 步长吸收）。
    #[must_use]
    pub fn pack(&self) -> [u8; LIGHT_RECORD_SIZE as usize] {
        let mut out = [0u8; LIGHT_RECORD_SIZE as usize];
        put_vec4(&mut out, 0, &self.pos_radius);
        put_vec4(&mut out, 16, &self.color_intensity);
        put_u32(&mut out, 32, self.flags);
        out
    }

    /// type 字段提取。
    #[must_use]
    pub fn light_type(&self) -> u32 {
        (self.flags >> LIGHT_TYPE_SHIFT) & 0xFF
    }
}

/// 光源常驻表：双帧槽 storage 环 + 槽位账本 + 脏行增量重写。
#[derive(bevy::prelude::Resource)]
pub struct LightTable {
    rings: Vec<GpuBuffer>,
    /// 槽位镜像（None = 空槽；census CPU 参照与 BRP 读数共用）。
    records: Vec<Option<LightRecord>>,
    /// 回收即复用（不走延迟回收，理由见模块注释）；游标 = cluster_build 遍历上界。
    slots: SlotAllocator,
    /// 脏行账：(槽号, 剩余帧槽写次数)。新脏 = 2（两个帧槽都要覆盖），
    /// 每帧槽写一次减一，归零出账。
    dirty: Vec<(u32, u8)>,
    dirty_writes_total: u64,
}

impl LightTable {
    /// 建表链：双帧槽 HOST_VISIBLE storage 环。失败路径由局部变量逆序 Drop。
    ///
    /// # Errors
    /// buffer 创建/内存分配失败。
    pub fn new(ctx: &crate::render::Context, contract: &MemoryContract) -> Result<Self> {
        let bytes = u64::from(LIGHT_TABLE_CAPACITY) * LIGHT_RECORD_SIZE;
        let mut rings = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            rings.push(GpuBuffer::create(
                &ctx.device,
                contract,
                bytes,
                BufferRole::Storage,
            )?);
        }
        info!(
            "光源常驻表就绪:{LIGHT_TABLE_CAPACITY} 槽 ×{LIGHT_RECORD_SIZE}B ×{MAX_FRAMES_IN_FLIGHT} 帧槽\
             (HOST_VISIBLE,脏行增量重写,fence 纪律同模型环;方向光不进表,set1 UBO 单光现役链保留)"
        );
        Ok(Self {
            rings,
            records: Vec::new(),
            slots: SlotAllocator::new(LIGHT_TABLE_CAPACITY),
            dirty: Vec::new(),
            dirty_writes_total: 0,
        })
    }

    /// 分配槽位并写记录（容量用尽报错不截断）。
    ///
    /// # Errors
    /// 容量耗尽。
    pub fn spawn(&mut self, record: LightRecord) -> Result<u32> {
        let slot = self.slots.alloc().ok_or_else(|| {
            VulkanError::Capacity(format!("光源表容量耗尽:{LIGHT_TABLE_CAPACITY} 槽——显式报错,不静默截断"))
        })?;
        self.records.resize(self.slots.high_water() as usize, None);
        self.records[slot as usize] = Some(record);
        self.mark_dirty(slot);
        Ok(slot)
    }

    /// 原位更新一盏灯的记录（槽号不变——cluster 列表引用稳定：改槽内容不换槽）。
    ///
    /// # Errors
    /// 槽号越界或槽位为空。
    pub fn update(&mut self, slot: u32, record: LightRecord) -> Result<()> {
        let entry = self
            .records
            .get_mut(slot as usize)
            .and_then(|e| e.as_mut())
            .ok_or_else(|| {
                VulkanError::Upload(format!("光源槽 {slot} 不在场——更新须指向已 spawn 的槽"))
            })?;
        *entry = record;
        self.mark_dirty(slot);
        Ok(())
    }

    /// 回收槽位：记录清零（死灯语义）+ 立即归还。槽内容随后续 spawn
    /// 复用；两个帧槽的旧内容由脏行账覆盖。
    ///
    /// # Errors
    /// 槽号越界或槽位已空。
    pub fn remove(&mut self, slot: u32) -> Result<()> {
        match self.records.get_mut(slot as usize) {
            Some(entry @ Some(_)) => {
                *entry = None;
            }
            _ => {
                return Err(VulkanError::Upload(format!(
                    "光源槽 {slot} 不在场——remove 须指向已 spawn 的槽"
                )));
            }
        }
        self.slots.release(slot);
        self.mark_dirty(slot);
        Ok(())
    }

    /// 在场灯数（cluster uniform 的 light_count 域 = 表遍历上界 high-water）。
    #[must_use]
    pub fn light_count(&self) -> u32 {
        self.slots.high_water()
    }

    /// 第 `slot` 帧槽环的 buffer 句柄（cluster 描述符绑定取数口——表本体
    /// 住本结构，消费方只持引用）。
    #[must_use]
    pub fn ring_buffer(&self, slot: usize) -> vk::Buffer {
        self.rings[slot % self.rings.len()].buffer()
    }

    /// 全表镜像只读访问（census CPU 参照：按槽号取，空槽 = 死灯语义）。
    #[must_use]
    pub fn record(&self, slot: u32) -> Option<LightRecord> {
        self.records.get(slot as usize).copied().flatten()
    }

    /// 在场槽号列表（BRP stats 读数面）。
    #[must_use]
    pub fn active_slots(&self) -> Vec<u32> {
        (0u32..)
            .zip(&self.records)
            .filter(|(_, e)| e.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// 脏行数（稳态零成本的验收读数口）。
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// 脏行写入累计（验收计量）。
    #[must_use]
    pub fn dirty_writes_total(&self) -> u64 {
        self.dirty_writes_total
    }

    /// 脏行增量重写到第 `frame` 帧槽（`wait_for_slot` 之后调用——fence 纪律
    /// 与模型环同源）。空脏集零写直接返回（稳态零成本）。
    ///
    /// # Errors
    /// 环写失败（Tier② 由调用方分流）。
    pub fn write_dirty(&mut self, frame: usize) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let n = self.rings.len();
        let ring = &mut self.rings[frame % n];
        for (slot, remaining) in &mut self.dirty {
            let bytes = self.records[*slot as usize]
                .unwrap_or(LightRecord::DEAD)
                .pack();
            ring.write(u64::from(*slot) * LIGHT_RECORD_SIZE, &bytes)?;
            *remaining = remaining.saturating_sub(1);
            self.dirty_writes_total += 1;
        }
        self.dirty.retain(|(_, remaining)| *remaining > 0);
        Ok(())
    }

    fn mark_dirty(&mut self, slot: u32) {
        match self.dirty.iter_mut().find(|(s, _)| *s == slot) {
            Some((_, remaining)) => *remaining = 2,
            None => self.dirty.push((slot, 2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LightRecord, LightTable, LIGHT_RECORD_SIZE, LIGHT_TABLE_CAPACITY};
    use crate::render::resident::slot_allocator::SlotAllocator;
    use crate::render::Context;

    /// 账本状态机（无 Vulkan 环境的纯账面部分）：spawn/update/remove/
    /// 脏行剩余写的语义。buffer 写路径归冒烟（构造需要 Context）。
    struct AccountHarness {
        records: Vec<Option<LightRecord>>,
        slots: SlotAllocator,
        dirty: Vec<(u32, u8)>,
    }

    impl AccountHarness {
        fn new() -> Self {
            Self { records: Vec::new(), slots: SlotAllocator::new(LIGHT_TABLE_CAPACITY), dirty: Vec::new() }
        }
        fn spawn(&mut self, record: LightRecord) -> u32 {
            let slot = self.slots.alloc().expect("测试规模远小于容量");
            self.records.resize(self.slots.high_water() as usize, None);
            self.records[slot as usize] = Some(record);
            self.mark_dirty(slot);
            slot
        }
        fn remove(&mut self, slot: u32) {
            self.records[slot as usize] = None;
            self.slots.release(slot);
            self.mark_dirty(slot);
        }
        fn mark_dirty(&mut self, slot: u32) {
            match self.dirty.iter_mut().find(|(s, _)| *s == slot) {
                Some((_, remaining)) => *remaining = 2,
                None => self.dirty.push((slot, 2)),
            }
        }
        /// 模拟 write_dirty 的一次帧槽写（只走账，不碰 buffer）。
        fn write_tick(&mut self) -> usize {
            let writes = self.dirty.len();
            for (_, remaining) in &mut self.dirty {
                *remaining = remaining.saturating_sub(1);
            }
            self.dirty.retain(|(_, remaining)| *remaining > 0);
            writes
        }
    }

    /// spawn → 两帧槽覆盖后脏账清空（稳态零写）；remove 复活脏行。
    #[test]
    fn dirty_ledger_two_slot_drain() {
        let mut h = AccountHarness::new();
        let slot = h.spawn(LightRecord {
            pos_radius: [1.0, 2.0, 3.0, 10.0],
            color_intensity: [1.0, 0.9, 0.8, 5.0],
            flags: 0,
        });
        assert_eq!(slot, 0);
        assert_eq!(h.dirty.len(), 1);
        assert_eq!(h.write_tick(), 1);
        assert_eq!(h.dirty.len(), 1, "首帧槽只减一次,脏行存活");
        assert_eq!(h.write_tick(), 1);
        assert_eq!(h.dirty.len(), 0, "两个帧槽都覆盖后出账");
        assert_eq!(h.write_tick(), 0, "稳态零写");
        h.remove(slot);
        assert_eq!(h.dirty.len(), 1, "回收槽复活脏行(死灯清零要写两个帧槽)");
        assert_eq!(h.slots.alloc(), Some(slot), "回收槽立即可复用");
    }

    /// 复用槽重新 spawn 后,同一槽的脏行剩余写重置回 2（换内容必须再次
    /// 覆盖两个帧槽）。
    #[test]
    fn reuse_slot_resets_remaining_writes() {
        let mut h = AccountHarness::new();
        let s0 = h.spawn(LightRecord::DEAD);
        h.remove(s0);
        h.write_tick();
        let s1 = h.spawn(LightRecord {
            pos_radius: [0.0; 4],
            color_intensity: [1.0, 1.0, 1.0, 1.0],
            flags: 0,
        });
        assert_eq!(s0, s1, "free list 优先复用");
        let entry = h.dirty.iter().find(|(s, _)| *s == s1).expect("脏行在账");
        assert_eq!(entry.1, 2, "复用 spawn 重置剩余写");
    }

    /// pack 布局抽查：64B、pos_radius/color_intensity/flags 同序、死灯全零。
    #[test]
    fn pack_layout() {
        let r = LightRecord {
            pos_radius: [1.0, 2.0, 3.0, 10.0],
            color_intensity: [0.5, 0.6, 0.7, 4.0],
            flags: 0x100,
        };
        let bytes = r.pack();
        assert_eq!(bytes.len(), LIGHT_RECORD_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(12), 10.0);
        assert_eq!(read_f32(28), 4.0);
        assert_eq!(
            u32::from_le_bytes(bytes[32..36].try_into().expect("4B")),
            0x100
        );
        // WGSL 垫区（36..64）保持 0——成员对齐把步长撑到 64，Rust 侧同布局
        assert_eq!(&bytes[36..], &[0u8; 28]);
        assert_eq!(LightRecord::DEAD.pack(), [0u8; 64]);
        assert_eq!(r.light_type(), 1, "flags bits 8..15 = type");
    }

    /// Context 依赖的构造签名编译性钉（真正的 buffer 写路径归冒烟）。
    #[test]
    fn table_type_shape() {
        fn _assert_resource(_: &LightTable) {}
        fn _new_signature(ctx: &Context, contract: &crate::render::MemoryContract) -> crate::common::error::Result<LightTable> {
            LightTable::new(ctx, contract)
        }
        let _ = _new_signature;
    }
}
