//! 只含 storage buffer 的描述符集：每帧槽一个集，binding 编号就是 buffer 在数组里的
//! 下标。布局、pool 容量与 binding 数都从 [`SetLayoutDesc`] 一份推导。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};

use super::descriptor_writer::DescriptorWriter;
use super::set_layout::SetLayoutDesc;

/// 建 pool 并逐槽分配、写入描述符集：`slots[i][b]` 写进第 i 集的 binding b。
/// pool 尺寸从声明推导；`desc` 必须是 0..N 连续单个 storage 的形状（[`SetLayoutDesc::dense_storage`]），
/// 且 `N` 与 `slots` 的行宽一致。分配失败时 pool 当场销毁，不留半成品。
///
/// # Safety
/// `layout` 未销毁且由 `desc.create` 建出；buffer 句柄须合法。
///
/// # Errors
/// `desc` 不是 dense storage 形状，或 pool 创建、描述符集分配失败。
pub(crate) unsafe fn create_sets<const N: usize>(
    device: &Device,
    desc: &'static SetLayoutDesc,
    layout: vk::DescriptorSetLayout,
    slots: &[[vk::Buffer; N]],
) -> Result<(vk::DescriptorPool, Vec<vk::DescriptorSet>)> {
    let n = desc.dense_storage().ok_or_else(|| {
        VulkanError::Init(format!(
            "描述符集布局 {} 不是 0..N 连续单个 storage 的形状，不能用 storage_sets",
            desc.name
        ))
    })?;
    if n != N as u32 || slots.is_empty() {
        return Err(VulkanError::Init(format!(
            "描述符集布局 {} 声明 {n} 个 storage binding，与槽位行宽 {} 不一致",
            desc.name, N
        )));
    }
    unsafe {
        let count = slots.len() as u32;
        let pool_sizes = desc.pool_sizes_vec(count);
        let pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default().max_sets(count).pool_sizes(&pool_sizes),
            None,
        )?;
        let layouts = vec![layout; slots.len()];
        let sets = match device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts),
        ) {
            Ok(sets) => sets,
            Err(e) => {
                device.destroy_descriptor_pool(pool, None);
                return Err(e.into());
            }
        };
        // 写入过声明校验（每个 binding 单个 STORAGE_BUFFER，写满才提交）
        for (set, buffers) in sets.iter().zip(slots) {
            let mut writer = DescriptorWriter::new(desc, *set);
            for (b, buffer) in buffers.iter().enumerate() {
                writer.buffer(b as u32, *buffer, 0, vk::WHOLE_SIZE);
            }
            writer.submit(device)?;
        }
        Ok((pool, sets))
    }
}
