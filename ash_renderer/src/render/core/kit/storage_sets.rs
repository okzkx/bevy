//! 只含 storage buffer 的描述符集：每帧槽一个集，binding 编号就是 buffer 在数组里的
//! 下标。布局由各资源族的 `SetLayoutDesc` 声明并创建。

use ash::{vk, Device};

use crate::common::error::Result;

/// 建 pool 并逐槽分配、写入描述符集：`slots[i][b]` 写进第 i 集的 binding b。
/// 分配失败时 pool 当场销毁，不留半成品。
///
/// # Safety
/// `layout` 未销毁，b0..N 都是单个 STORAGE_BUFFER；buffer 句柄须合法。
///
/// # Errors
/// pool 创建或描述符集分配失败。
pub(crate) unsafe fn create_sets<const N: usize>(
    device: &Device,
    layout: vk::DescriptorSetLayout,
    slots: &[[vk::Buffer; N]],
) -> Result<(vk::DescriptorPool, Vec<vk::DescriptorSet>)> {
    unsafe {
        let count = slots.len() as u32;
        let pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(count)
                .pool_sizes(&[vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(N as u32 * count)]),
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
        let infos: Vec<vk::DescriptorBufferInfo> = slots
            .iter()
            .flatten()
            .map(|buffer| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = sets
            .iter()
            .zip(infos.chunks(N))
            .flat_map(|(set, infos)| {
                infos.iter().enumerate().map(|(b, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(b as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(std::slice::from_ref(info))
                })
            })
            .collect();
        device.update_descriptor_sets(&writes, &[]);
        Ok((pool, sets))
    }
}
