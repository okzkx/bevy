//! GPU 环的 host 侧读写工具：标记/档位/缺口环共用的整段填充、读回解包与计数。
//! 调用方都在 wait_for_slot 的 fence 保护内（culling/lod/gaps 各备料单元）。

use crate::{common::error::VulkanError, render::GpuBuffer};

/// host 侧整段填充 u32 环：`count` 个小端 `value`（开关关闭时的全可见/全最细档）。
pub(super) fn fill_u32(ring: &mut GpuBuffer, count: usize, value: u32) -> Result<(), VulkanError> {
    ring.write(0, &value.to_le_bytes().repeat(count))
}

/// 读 u32 环前 `len` 字节并按小端解包（标记/档位/缺口环同款）：尾部不足 4B 的
/// 残字节丢弃，调用方按槽 `.get()` 取值，越界视同缺席。
pub(super) fn read_ring(ring: &mut GpuBuffer, len: usize) -> Result<Vec<u32>, VulkanError> {
    let mut bytes = vec![0u8; len];
    ring.read(0, &mut bytes)?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

/// 在场行中标记 = 1 的行数（剔除/遮挡诊断读数）。
pub(super) fn count_marked(marks: &[u32], live: &[u32]) -> u32 {
    live.iter()
        .filter(|&&slot| marks.get(slot as usize) == Some(&1))
        .count() as u32
}
