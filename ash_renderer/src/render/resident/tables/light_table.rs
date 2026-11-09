//! 光源常驻表：点光源的常驻 storage 表，按槽位增量更新（与模型环同一套脏行做法）。
//!
//! - **按帧槽各一份 + fence 保护**：表本体 HOST_VISIBLE ×`MAX_FRAMES_IN_FLIGHT`，脏行在
//!   `wait_for_slot` 之后才写，宿主写与 GPU 读不会同时落在同一份内存上。灯没变化时零
//!   上传；改动的行要两个帧槽都写过才出账（换帧槽后另一份还是旧内容）。
//! - **槽号跨帧稳定**：cluster 的逐 tile 光源列表存的是槽号。回收的槽立即清零并可复用
//!   （radius=0 即死灯，cluster 求交与着色都按 `r > 0` 跳过），不设延迟回收：旧列表在
//!   1 帧内引用到换了内容的槽，最坏闪一下，不会读到无效数据；灯量在百以内，不值得上
//!   实例表那套延迟回收。
//! - **方向光不进表**：方向光走 set1 UBO；表只装点光（type 字段为 spot 预留）。
//!
//! shadow 标志位（flags bit0）本表只记录不使用，由阴影采样侧决定。

use ash::vk;
use bevy::log::info;

use crate::common::bytes::{put_u32, put_vec4};
use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::MemoryContract;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::resident::slot_allocator::SlotAllocator;
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 光源表容量（槽位数）。
pub(crate) const LIGHT_TABLE_CAPACITY: u32 = 256;

/// 光源记录字节数：vec4×2 + flags + 垫 = 64B。与 WGSL `LightRecord` 一致——WGSL 侧
/// `_pad: vec3<u32>` 的对齐把步长撑到 64（pad 在 @48），Rust 侧 36..64 保持 0。
pub(crate) const LIGHT_RECORD_SIZE: u64 = 64;

/// 光源类型（flags bits 8..15）：只实现了点光；spot 只预留记录形状（锥体求交未实现）。
pub const LIGHT_TYPE_POINT: u32 = 0;
#[expect(dead_code, reason = "spot 未实现；记录形状预留（type 域契约）")]
pub const LIGHT_TYPE_SPOT: u32 = 1;

pub const LIGHT_FLAG_SHADOW: u32 = 1 << 0;
pub const LIGHT_TYPE_SHIFT: u32 = 8;

/// 一条光源记录（[`LightTable::spawn`] 的入参，也是 CPU 侧参照）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightRecord {
    /// 世界空间位置（xyz）+ 作用半径（w，米；0 = 死灯，cluster 与着色都跳过）。
    pub pos_radius: [f32; 4],
    /// 颜色（rgb 线性）× 强度（w，candela；曝光换算归消费端）。
    pub color_intensity: [f32; 4],
    /// bit0 = shadow；bits 8..15 = type（[`LIGHT_TYPE_POINT`]）。
    pub flags: u32,
}

impl LightRecord {
    /// 死灯：空槽写进 GPU 的内容。
    pub const DEAD: Self = Self {
        pos_radius: [0.0; 4],
        color_intensity: [0.0; 4],
        flags: 0,
    };

    /// 打包成 64B：pos @0 / color @16 / flags @32，其余字节保持 0。
    #[must_use]
    pub fn pack(&self) -> [u8; LIGHT_RECORD_SIZE as usize] {
        let mut out = [0u8; LIGHT_RECORD_SIZE as usize];
        put_vec4(&mut out, 0, &self.pos_radius);
        put_vec4(&mut out, 16, &self.color_intensity);
        put_u32(&mut out, 32, self.flags);
        out
    }

    #[must_use]
    pub fn light_type(&self) -> u32 {
        (self.flags >> LIGHT_TYPE_SHIFT) & 0xFF
    }
}

/// 光源常驻表：按帧槽各一份的 storage buffer + 槽位与脏行账。
#[derive(bevy::prelude::Resource)]
pub struct LightTable {
    rings: Vec<GpuBuffer>,
    ledger: LightLedger,
}

impl LightTable {
    /// 每帧槽建一份 HOST_VISIBLE storage buffer。失败时已建的 buffer 随局部变量 Drop。
    ///
    /// # Errors
    /// buffer 创建/内存分配失败。
    pub fn new(ctx: &crate::render::Context, contract: &MemoryContract) -> Result<Self> {
        let bytes = u64::from(LIGHT_TABLE_CAPACITY) * LIGHT_RECORD_SIZE;
        let rings = (0..MAX_FRAMES_IN_FLIGHT)
            .map(|_| GpuBuffer::create(&ctx.device, contract, bytes, BufferRole::Storage))
            .collect::<Result<_>>()?;
        info!(
            "光源常驻表就绪:{LIGHT_TABLE_CAPACITY} 槽 ×{LIGHT_RECORD_SIZE}B ×{MAX_FRAMES_IN_FLIGHT} 帧槽\
             (HOST_VISIBLE,脏行增量重写,fence 纪律同模型环;方向光不进表,set1 UBO 单光现役链保留)"
        );
        Ok(Self {
            rings,
            ledger: LightLedger::new(),
        })
    }

    /// 分配槽位并写记录。
    ///
    /// # Errors
    /// 容量耗尽（报错，不截断）。
    pub fn spawn(&mut self, record: LightRecord) -> Result<u32> {
        self.ledger.spawn(record)
    }

    /// 原位更新一盏灯（槽号不变，cluster 列表里的引用保持有效）。
    ///
    /// # Errors
    /// 槽号越界或槽位为空。
    pub fn update(&mut self, slot: u32, record: LightRecord) -> Result<()> {
        self.ledger.update(slot, record)
    }

    /// 回收槽位：立即归还可复用；两个帧槽里的旧内容由脏行账改写成死灯。
    ///
    /// # Errors
    /// 槽号越界或槽位已空。
    pub fn remove(&mut self, slot: u32) -> Result<()> {
        self.ledger.remove(slot)
    }

    /// cluster uniform 的 light_count：表遍历上界（用过的最大槽号 + 1，含空槽）。
    #[must_use]
    pub fn light_count(&self) -> u32 {
        self.ledger.slots.high_water()
    }

    /// 第 `slot` 帧槽的 buffer 句柄（cluster 描述符绑定用）。
    #[must_use]
    pub fn ring_buffer(&self, slot: usize) -> vk::Buffer {
        self.rings[slot % self.rings.len()].buffer()
    }

    /// 按槽号取 CPU 侧镜像；空槽 None。
    #[must_use]
    pub fn record(&self, slot: u32) -> Option<LightRecord> {
        self.ledger.records.get(slot as usize).copied().flatten()
    }

    /// 在用的槽号列表（BRP 读数）。
    #[must_use]
    pub fn active_slots(&self) -> Vec<u32> {
        (0u32..)
            .zip(&self.ledger.records)
            .filter(|(_, e)| e.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// 待写的脏行数（灯没变化时应为 0）。
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.ledger.dirty.len()
    }

    /// 脏行累计写入次数。
    #[must_use]
    pub fn dirty_writes_total(&self) -> u64 {
        self.ledger.dirty_writes_total
    }

    /// 把脏行写进第 `frame` 帧槽，须在 `wait_for_slot` 之后调用。没有脏行直接返回。
    ///
    /// # Errors
    /// buffer 写入失败。
    pub fn write_dirty(&mut self, frame: usize) -> Result<()> {
        let n = self.rings.len();
        let ring = &mut self.rings[frame % n];
        self.ledger
            .flush_dirty(|offset, bytes| ring.write(offset, bytes))
            .map(|_| ())
    }
}

/// 槽位镜像 + 脏行账：纯 CPU 账面，与 GPU buffer 分开，单测直接测它。
struct LightLedger {
    /// 槽位镜像（None = 空槽）。
    records: Vec<Option<LightRecord>>,
    /// 回收即复用（不走延迟回收，理由见模块注释）。
    slots: SlotAllocator,
    /// 脏行账：(槽号, 剩余要写的帧槽数)。新脏 = 2，每写一个帧槽减一，归零出账。
    dirty: Vec<(u32, u8)>,
    dirty_writes_total: u64,
}

impl LightLedger {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            slots: SlotAllocator::new(LIGHT_TABLE_CAPACITY),
            dirty: Vec::new(),
            dirty_writes_total: 0,
        }
    }

    fn spawn(&mut self, record: LightRecord) -> Result<u32> {
        let slot = self.slots.alloc().ok_or_else(|| {
            VulkanError::Capacity(format!(
                "光源表容量耗尽:{LIGHT_TABLE_CAPACITY} 槽——显式报错,不静默截断"
            ))
        })?;
        self.records.resize(self.slots.high_water() as usize, None);
        self.records[slot as usize] = Some(record);
        self.mark_dirty(slot);
        Ok(slot)
    }

    fn update(&mut self, slot: u32, record: LightRecord) -> Result<()> {
        let entry = self
            .records
            .get_mut(slot as usize)
            .and_then(|e| e.as_mut())
            .ok_or_else(|| {
                VulkanError::Contract(format!("光源槽 {slot} 不在场——更新须指向已 spawn 的槽"))
            })?;
        *entry = record;
        self.mark_dirty(slot);
        Ok(())
    }

    fn remove(&mut self, slot: u32) -> Result<()> {
        match self.records.get_mut(slot as usize) {
            Some(entry @ Some(_)) => *entry = None,
            _ => {
                return Err(VulkanError::Contract(format!(
                    "光源槽 {slot} 不在场——remove 须指向已 spawn 的槽"
                )));
            }
        }
        self.slots.release(slot);
        self.mark_dirty(slot);
        Ok(())
    }

    fn mark_dirty(&mut self, slot: u32) {
        match self.dirty.iter_mut().find(|(s, _)| *s == slot) {
            Some((_, remaining)) => *remaining = 2,
            None => self.dirty.push((slot, 2)),
        }
    }

    /// 把每条脏行（空槽写死灯）交给 `write(偏移, 字节)` 写进一个帧槽，剩余次数减一、
    /// 归零出账。返回本次写入行数。
    fn flush_dirty(&mut self, mut write: impl FnMut(u64, &[u8]) -> Result<()>) -> Result<usize> {
        if self.dirty.is_empty() {
            return Ok(0);
        }
        for (slot, remaining) in &mut self.dirty {
            let bytes = self.records[*slot as usize]
                .unwrap_or(LightRecord::DEAD)
                .pack();
            write(u64::from(*slot) * LIGHT_RECORD_SIZE, &bytes)?;
            *remaining = remaining.saturating_sub(1);
            self.dirty_writes_total += 1;
        }
        let written = self.dirty.len();
        self.dirty.retain(|(_, remaining)| *remaining > 0);
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::{LightLedger, LightRecord, LIGHT_RECORD_SIZE};

    fn tick(ledger: &mut LightLedger) -> usize {
        ledger.flush_dirty(|_, _| Ok(())).unwrap()
    }

    /// spawn → 两个帧槽都写过后脏账清空（之后零写）；remove 让该行重新变脏。
    #[test]
    fn dirty_ledger_two_slot_drain() {
        let mut ledger = LightLedger::new();
        let slot = ledger
            .spawn(LightRecord {
                pos_radius: [1.0, 2.0, 3.0, 10.0],
                color_intensity: [1.0, 0.9, 0.8, 5.0],
                flags: 0,
            })
            .unwrap();
        assert_eq!(slot, 0);
        assert_eq!(ledger.dirty.len(), 1);
        assert_eq!(tick(&mut ledger), 1);
        assert_eq!(ledger.dirty.len(), 1, "只写了一个帧槽,脏行还在");
        assert_eq!(tick(&mut ledger), 1);
        assert_eq!(ledger.dirty.len(), 0, "两个帧槽都写过后脏行清空");
        assert_eq!(tick(&mut ledger), 0, "无变化时零写");
        assert_eq!(ledger.dirty_writes_total, 2);
        ledger.remove(slot).unwrap();
        assert_eq!(ledger.dirty.len(), 1, "回收槽重新变脏(死灯要写两个帧槽)");
        assert_eq!(ledger.slots.alloc(), Some(slot), "回收槽立即可复用");
    }

    /// 复用槽重新 spawn 后，该槽剩余写次数重置回 2（换内容要再覆盖两个帧槽）。
    #[test]
    fn reuse_slot_resets_remaining_writes() {
        let mut ledger = LightLedger::new();
        let s0 = ledger.spawn(LightRecord::DEAD).unwrap();
        ledger.remove(s0).unwrap();
        tick(&mut ledger);
        let s1 = ledger
            .spawn(LightRecord {
                pos_radius: [0.0; 4],
                color_intensity: [1.0, 1.0, 1.0, 1.0],
                flags: 0,
            })
            .unwrap();
        assert_eq!(s0, s1, "优先复用回收槽");
        let entry = ledger
            .dirty
            .iter()
            .find(|(s, _)| *s == s1)
            .expect("脏行在账");
        assert_eq!(entry.1, 2, "复用 spawn 重置剩余写");
    }

    /// 空槽与越界槽的 update/remove 报错。
    #[test]
    fn update_and_remove_reject_empty_slot() {
        let mut ledger = LightLedger::new();
        assert!(ledger.update(0, LightRecord::DEAD).is_err());
        assert!(ledger.remove(0).is_err());
        let slot = ledger.spawn(LightRecord::DEAD).unwrap();
        ledger.remove(slot).unwrap();
        assert!(ledger.remove(slot).is_err(), "重复 remove 报错");
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
        // 36..64 保持 0（WGSL 侧对齐把步长撑到 64）
        assert_eq!(&bytes[36..], &[0u8; 28]);
        assert_eq!(LightRecord::DEAD.pack(), [0u8; 64]);
        assert_eq!(r.light_type(), 1, "flags bits 8..15 = type");
    }
}
