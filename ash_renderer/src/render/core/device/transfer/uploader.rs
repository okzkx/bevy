//! 合批提交设备传输，并用 timeline 票据跟踪完成状态。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::common::error::{Result, VulkanError};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::MemoryContract;
use crate::render::core::device::memory::role::BufferRole;

use super::types::{CopyRegion, Ticket, UploadBatch};

mod image;

/// staging buffer、命令缓冲及该槽最后一次提交的票据。
struct RingSlot {
    staging: Option<GpuBuffer>,
    command_buffer: vk::CommandBuffer,
    /// 与本 transfer 槽配对、供 graphics 队列生成 mip 的命令缓冲。
    blit_command_buffer: vk::CommandBuffer,
    in_flight: Option<Ticket>,
}

/// 持有 transfer 和 graphics 队列、命令池、staging 槽及完成信号量。
#[derive(bevy::prelude::Resource)]
pub struct Uploader {
    device: Device,
    queue_family: u32,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    /// mip blit 在 graphics 队列录制；续批等待 transfer 票据，完成后发最终票据。
    graphics_queue: vk::Queue,
    blit_pool: vk::CommandPool,
    slots: Vec<RingSlot>,
    cursor: usize,
    /// 保存已完成票据的时间线信号量。
    ticket_semaphore: vk::Semaphore,
    /// 下一个要 signal 的票据；0 保留给空批次。
    next_ticket: Ticket,
    /// 每个槽位的 staging 初始容量，空间不足时再扩容。
    staging_capacity: u64,
    /// 扩容 staging 槽时使用的设备内存属性。
    contract: MemoryContract,
}

impl Uploader {
    /// 创建两类队列的命令池和缓冲、时间线信号量；staging 按需分配。
    ///
    /// # Errors
    /// 命令池、命令缓冲或信号量创建失败。
    #[expect(
        clippy::too_many_arguments,
        reason = "参数分别说明 transfer 和 graphics 队列配置。"
    )]
    pub fn new(
        device: &Device,
        contract: &MemoryContract,
        queue_family: u32,
        queue: vk::Queue,
        graphics_family: u32,
        graphics_queue: vk::Queue,
        staging_capacity: u64,
        slots: usize,
    ) -> Result<Self> {
        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 命令缓冲会逐个重录，因此使用 RESET_COMMAND_BUFFER。
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(queue_family),
                None,
            )
        }?;
        // mip blit 在 graphics 队列提交，使用 graphics 队列族的命令池。
        let blit_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(graphics_family),
                None,
            )
        }?;
        let command_buffers = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(slots as u32),
            )
        }?;
        let blit_command_buffers = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(blit_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(slots as u32),
            )
        }?;
        let result = unsafe {
            Self::new_inner(
                device,
                contract,
                queue_family,
                queue,
                graphics_queue,
                staging_capacity,
                command_pool,
                blit_pool,
                &command_buffers,
                &blit_command_buffers,
            )
        };
        if result.is_err() {
            unsafe {
                device.destroy_command_pool(command_pool, None);
                device.destroy_command_pool(blit_pool, None);
            };
        }
        result
    }

    /// 命令池和缓冲创建后，继续创建信号量并初始化槽位。
    ///
    /// # Safety
    /// 传入的命令池和命令缓冲必须有效且未销毁。
    #[expect(clippy::too_many_arguments, reason = "参数是 new 创建链的后半段。")]
    unsafe fn new_inner(
        device: &Device,
        contract: &MemoryContract,
        queue_family: u32,
        queue: vk::Queue,
        graphics_queue: vk::Queue,
        staging_capacity: u64,
        command_pool: vk::CommandPool,
        blit_pool: vk::CommandPool,
        command_buffers: &[vk::CommandBuffer],
        blit_command_buffers: &[vk::CommandBuffer],
    ) -> Result<Self> {
        // Timeline semaphore 功能在创建设备时启用。
        let mut timeline = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let semaphore_info = vk::SemaphoreCreateInfo::default().push_next(&mut timeline);
        let ticket_semaphore = unsafe { device.create_semaphore(&semaphore_info, None) }?;
        Ok(Self {
            device: device.clone(),
            queue_family,
            queue,
            command_pool,
            slots: command_buffers
                .iter()
                .zip(blit_command_buffers)
                .map(|(&command_buffer, &blit_command_buffer)| RingSlot {
                    staging: None,
                    command_buffer,
                    blit_command_buffer,
                    in_flight: None,
                })
                .collect(),
            cursor: 0,
            graphics_queue,
            blit_pool,
            ticket_semaphore,
            next_ticket: 1,
            staging_capacity,
            contract: contract.clone(),
        })
    }

    /// 已发出的最高票据。
    #[must_use]
    pub fn last_issued_ticket(&self) -> Ticket {
        self.next_ticket - 1
    }

    /// 传输队列族索引。
    #[must_use]
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }

    /// GPU 已完成的最高票据。
    ///
    /// # Errors
    /// `vkGetSemaphoreCounterValue` 失败(设备丢失等)。
    pub fn completed_ticket(&self) -> Result<Ticket> {
        unsafe {
            self.device
                .get_semaphore_counter_value(self.ticket_semaphore)
        }
        .map_err(VulkanError::from)
    }

    /// 图形提交等待上传完成时使用的信号量。
    #[must_use]
    pub fn ticket_semaphore(&self) -> vk::Semaphore {
        self.ticket_semaphore
    }

    /// 等待指定票据及此前提交的传输完成。
    ///
    /// # Errors
    /// 等待超过 5 秒或 Vulkan 调用失败。
    pub fn wait_until(&self, ticket: Ticket) -> Result<()> {
        if self.completed_ticket()? >= ticket {
            return Ok(());
        }
        let values = [ticket];
        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(slice::from_ref(&self.ticket_semaphore))
            .values(&values);
        unsafe { self.device.wait_semaphores(&wait, 5_000_000_000)? };
        Ok(())
    }

    /// 等待所有已发出票据完成。
    ///
    /// # Errors
    /// 同 [`Self::wait_until`]。
    pub fn wait_all_uploads(&self) -> Result<()> {
        self.wait_until(self.last_issued_ticket())
    }

    /// 提交一批传输，非空批次返回完成票据。
    ///
    /// # Errors
    /// staging 扩容或写入、命令录制或队列提交失败。
    pub fn submit_batch(&mut self, batch: UploadBatch) -> Result<Option<Ticket>> {
        let staging_len = batch.staging.len() as u64;
        if staging_len == 0
            && batch.uploads.is_empty()
            && batch.device_copies.is_empty()
            && batch.image_uploads.is_empty()
        {
            return Ok(None);
        }
        let index = self.cursor;
        self.cursor = (self.cursor + 1) % self.slots.len();
        let pending = self.slots[index].in_flight;
        let completed = self.completed_ticket()?;
        if let Some(p) = pending.filter(|p| completed < *p) {
            self.wait_until(p)?;
            info!("staging 槽位 {index} 复用:等上批票据 {p} 完成后重录(按完成票据复用)");
        }
        let need_new = self.slots[index]
            .staging
            .as_ref()
            .is_none_or(|s| s.allocation_size() < staging_len);
        if need_new {
            let grown = GpuBuffer::create(
                &self.device,
                &self.contract,
                staging_len.max(self.staging_capacity),
                BufferRole::Staging,
            )?;
            info!(
                "staging 槽位 {index} 扩到 {}B(请求 {staging_len}B,上批票据已完成)",
                grown.allocation_size()
            );
            self.slots[index].staging = Some(grown);
        }
        let ticket = self.next_ticket;
        let queue = self.queue;
        let queue_family = self.queue_family;
        let semaphore = self.ticket_semaphore;
        let device = &self.device;
        let slot = &mut self.slots[index];
        // 先写入 staging，再录制读取该映射内存的拷贝。
        let Some(staging_buffer) = slot.staging.as_mut() else {
            // 容量检查通过后 staging 槽必须已创建。
            return Err(VulkanError::Upload(
                "内部矛盾:扩容判定后 staging 槽位仍缺席".into(),
            ));
        };
        staging_buffer.write(0, &batch.staging)?;
        unsafe {
            device
                .reset_command_buffer(slot.command_buffer, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                slot.command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            for region in &batch.uploads {
                // 调用方保证偏移与长度按 4 字节对齐，且缓冲区具备传输用途。
                device.cmd_copy_buffer(
                    slot.command_buffer,
                    staging_buffer.buffer(),
                    region.dst,
                    &[vk::BufferCopy {
                        src_offset: region.src_offset,
                        dst_offset: region.dst_offset,
                        size: region.size,
                    }],
                );
            }
            if !batch.image_uploads.is_empty() {
                image::record_image_uploads(
                    device,
                    slot.command_buffer,
                    queue_family,
                    staging_buffer.buffer(),
                    &batch,
                );
            }
            if !batch.device_copies.is_empty() {
                // 屏障覆盖本批 transfer 写入；后续 transfer 读取在此建立可见性。
                for buffer in distinct_srcs(&batch.device_copies) {
                    let hazard = vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(buffer)
                        .size(vk::WHOLE_SIZE);
                    device.cmd_pipeline_barrier(
                        slot.command_buffer,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[hazard],
                        &[],
                    );
                }
                for region in &batch.device_copies {
                    device.cmd_copy_buffer(
                        slot.command_buffer,
                        region.src,
                        region.dst,
                        &[vk::BufferCopy {
                            src_offset: region.src_offset,
                            dst_offset: region.dst_offset,
                            size: region.size,
                        }],
                    );
                }
            }
            for release in &batch.releases {
                // release 交出 transfer 写入；目标队列的 acquire 负责后续访问。
                let owned = vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .src_queue_family_index(queue_family)
                    .dst_queue_family_index(release.to_family)
                    .buffer(release.buffer)
                    .size(vk::WHOLE_SIZE);
                device.cmd_pipeline_barrier(
                    slot.command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[owned],
                    &[],
                );
            }
            device.end_command_buffer(slot.command_buffer)?;

            // signal 票据数须与信号量数一致。
            let mut timeline_submit = vk::TimelineSemaphoreSubmitInfo::default()
                .signal_semaphore_values(slice::from_ref(&ticket));
            let cbs = slice::from_ref(&slot.command_buffer);
            let signals = slice::from_ref(&semaphore);
            device.queue_submit(
                queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(cbs)
                    .signal_semaphores(signals)
                    .push_next(&mut timeline_submit)],
                vk::Fence::null(),
            )?;
        }
        // transfer 提交成功后才发布票据。
        slot.in_flight = Some(ticket);
        self.next_ticket += 1;
        // 多级图像由续批等待该票据，生成完整 mip 链后再发布新票据。
        let chained = image::chained_images(&batch.image_uploads);
        if chained.is_empty() {
            return Ok(Some(ticket));
        }
        let blit_cb = self.slots[index].blit_command_buffer;
        image::record_blit_continuation(self, blit_cb, ticket, &chained)?;
        let published = ticket + 1;
        self.slots[index].in_flight = Some(published);
        self.next_ticket = published + 1;
        info!("mip 生成续批:graphics 队列 blit {} 张(transfer 票据 #{ticket} → 发布票据 #{published})", chained.len());
        Ok(Some(published))
    }
}

/// 去重本批 device copies 的源 buffer。
fn distinct_srcs(regions: &[CopyRegion]) -> Vec<vk::Buffer> {
    let mut seen = Vec::new();
    for region in regions {
        if !seen.contains(&region.src) {
            seen.push(region.src);
        }
    }
    seen
}

impl Drop for Uploader {
    fn drop(&mut self) {
        // 退出前会先等待设备空闲，再销毁上传器资源。
        unsafe {
            self.device.destroy_semaphore(self.ticket_semaphore, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_command_pool(self.blit_pool, None);
        }
    }
}
