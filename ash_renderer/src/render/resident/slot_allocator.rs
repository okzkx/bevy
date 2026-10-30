//! 槽位分配器：给 GPU 表里的行分配下标。纯 CPU 账面，不碰 Vulkan。
//!
//! 分配先取回收栈，栈空再推进游标；容量用完返回 `None`，报错文案由各表自己写。
//! 归还分两种，按"GPU 还会不会读旧内容"选：
//! - [`SlotAllocator::release`]：立即可复用。用于从没发布给 GPU 的槽，或每个帧槽
//!   各有一份数据、旧帧读的是自己那份的表。
//! - [`SlotAllocator::retire`]：先进待回收账，调用方等 GPU 用完后再
//!   [`SlotAllocator::drain_retired`] 归还。在飞的 draw 可能还在读旧内容时必须用这个。

pub(crate) struct SlotAllocator {
    capacity: u32,
    /// 游标：从没分配过的最小槽号。复用回收槽不推进它。
    high_water: u32,
    free: Vec<u32>,
    retired: Vec<u32>,
}

impl SlotAllocator {
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            capacity,
            high_water: 0,
            free: Vec::new(),
            retired: Vec::new(),
        }
    }

    /// 分配一个槽；容量用完返回 `None`。
    pub(crate) fn alloc(&mut self) -> Option<u32> {
        if let Some(slot) = self.free.pop() {
            return Some(slot);
        }
        (self.high_water < self.capacity).then(|| {
            self.high_water += 1;
            self.high_water - 1
        })
    }

    /// 立即归还，下次 `alloc` 就可能拿到它。
    pub(crate) fn release(&mut self, slot: u32) {
        debug_assert!(slot < self.high_water, "归还了从没分配过的槽 {slot}");
        self.free.push(slot);
    }

    /// 延迟归还：在 [`Self::drain_retired`] 之前不会被再次分配。
    pub(crate) fn retire(&mut self, slot: u32) {
        debug_assert!(slot < self.high_water, "回收了从没分配过的槽 {slot}");
        self.retired.push(slot);
    }

    /// 把待回收的槽全部归还，返回归还数。调用方须先确认 GPU 已不再读这些槽。
    pub(crate) fn drain_retired(&mut self) -> usize {
        let count = self.retired.len();
        self.free.append(&mut self.retired);
        count
    }

    /// 分配过的最大槽号 + 1（含已回收的槽）。表的有效范围是 `0..high_water`。
    pub(crate) fn high_water(&self) -> u32 {
        self.high_water
    }

    pub(crate) fn capacity(&self) -> u32 {
        self.capacity
    }

    pub(crate) fn retired_count(&self) -> usize {
        self.retired.len()
    }
}

#[cfg(test)]
mod tests {
    use super::SlotAllocator;

    #[test]
    fn bump_until_full() {
        let mut slots = SlotAllocator::new(2);
        assert_eq!(slots.alloc(), Some(0));
        assert_eq!(slots.alloc(), Some(1));
        assert_eq!(slots.alloc(), None, "容量用完不越界");
        assert_eq!(slots.high_water(), 2);
    }

    #[test]
    fn release_reuses_before_bump() {
        let mut slots = SlotAllocator::new(4);
        let a = slots.alloc().expect("有容量");
        let _b = slots.alloc().expect("有容量");
        slots.release(a);
        assert_eq!(slots.alloc(), Some(a), "先取回收栈");
        assert_eq!(slots.high_water(), 2, "复用不推进游标");
    }

    #[test]
    fn retired_slot_waits_for_drain() {
        let mut slots = SlotAllocator::new(1);
        let a = slots.alloc().expect("有容量");
        slots.retire(a);
        assert_eq!(slots.retired_count(), 1);
        assert_eq!(slots.alloc(), None, "排空前不能复用");
        assert_eq!(slots.drain_retired(), 1);
        assert_eq!(slots.retired_count(), 0);
        assert_eq!(slots.alloc(), Some(a));
    }
}
