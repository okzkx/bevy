//! 上传编排(3.2.4):staging 环 + transfer 提交 + 单调 timeline 票据。
//!
//! 一次合批 = 一个 staging 范围 + 一次 transfer 提交(多段 `vkCmdCopyBuffer`
//! regions + 单 signal),**不逐批新建 staging 对象**——staging 与 transfer 命令
//! 缓冲按 2 槽位轮转复用(3.2.4.3)。复用安全由票据证明:每槽记住最后提交的
//! 票据值,复用前查 timeline 计数,未完成则 CPU 等待——"按完成票据复用资源
//! 所需的 fence/timeline 等待是正常同步"(施工计划 §0 判定线 6),不冒充
//! "正常上传零 idle",每次等待都有日志。
//!
//! **票据语义**(3.2.4.1):提交成功后 signal `ticket_semaphore` 的新值
//! `next_ticket`(从 1 起单调 +1,满足 VUID-VkSubmitInfo-pSignalSemaphores-03242
//! "signal 值须大于当前值")。提交挂 fence(null)——完成只由 timeline 表达,
//! CPU 等待只发生在复用/维护/退出路径,不在逐批路径上。空批次(无 staging 写、
//! 无拷贝)不提交、不发票据(返回 `None`);任何失败(重置/录制/提交)在改写
//! `next_ticket` 之前早退,不发布票据、不占槽——"失败不发布虚假完成票据"
//! (3.2.4.4)从结构上成立。
//!
//! **timeline 提交接口定案**(面板口径):保留旧式 SubmitInfo,timeline 信号量
//! 进提交时按 VUID-VkSubmitInfo-pNext-03239 挂 `TimelineSemaphoreSubmitInfo`;
//! 不启用 synchronization2(设备创建也从未开它),两套同步接口不混用。
//!
//! **跨族所有权**(3.2.4.2,EXCLUSIVE 资源):本机有纯 transfer 族,拷贝提交在
//! transfer 队列、图形消费在 graphics 队列——拷贝写完后在**同一提交**末尾记录
//! **release**(src=transfer 族,dst=graphics 族;规范明言 release 的 dst 阶段/
//! 访问掩码被忽略,填 BOTTOM_OF_PIPE/空只满足旧式屏障非零前提,依据
//! sync.adoc §Queue Family Ownership Transfer)。配对的 **acquire** 记在图形族
//! 的提交里(acquire 的 srcAccessMask 同样被忽略,src 阶段按规范用
//! ALL_COMMANDS 等待 release 完成)——M2 帧循环只清屏,图形尚不消费池数据,
//! 故正常路径 `releases` 为空、acquire 机制由探针跨族组全链实证,3.4 接 draw
//! 时按同一形状接线。同族回退(无专用 transfer 族的设备)无所有权转移,但
//! "等票据 + TRANSFER→消费屏障"的内存依赖仍在,同族形状已由 memory_probe
//! 闭环(零 VUID)覆盖。

use std::slice;

use ash::{vk, Device};
use bevy::log::info;

use crate::error::VulkanError;

use super::resources::{BufferRole, GpuBuffer, MemoryContract};

/// 上传完成票据:timeline 信号量的单调值。0 专表"空批"(永不 signal)。
pub type Ticket = u64;

/// 一段设备侧拷贝(src/dst buffer + 偏移,字节;偏移与 size 须为 4 的倍数,
/// 见 VUID-vkCmdCopyBuffer-srcOffset-00113/00114 与对齐纪律)。
#[derive(Clone, Copy, Debug)]
pub struct CopyRegion {
    pub src: vk::Buffer,
    pub dst: vk::Buffer,
    pub src_offset: u64,
    pub dst_offset: u64,
    pub size: u64,
}

/// 一段 staging → 池的拷贝:src 恒为本批 staging(由 uploader 自持,调用方
/// 只给 staging 内偏移),dst 是目标池 buffer。
#[derive(Clone, Copy, Debug)]
pub struct StagingCopy {
    /// 目标池 buffer(顶点池或索引池)。
    pub dst: vk::Buffer,
    /// 本批 staging 内的源偏移。
    pub src_offset: u64,
    pub dst_offset: u64,
    pub size: u64,
}

/// 异族 release:一个 buffer 的所有权从 transfer 族让渡给目标族。
#[derive(Clone, Copy, Debug)]
pub struct Release {
    pub buffer: vk::Buffer,
    /// 接收所有权的队列族(3.4 起 = graphics 族)。
    pub to_family: u32,
}

/// 一段 staging → image 的拷贝:src 恒为本批 staging,dst 是 2D mip0 单层(3.3.1
/// 静态 mip0 形状)。字节排布按紧 packing(`bufferRowLength/bufferImageHeight = 0`,
/// 行距由驱动按格式推导),与 bevy `Image.data` 的 mip0 布局一致。
#[derive(Clone, Copy, Debug)]
pub struct StagingImageCopy {
    /// 目标 VkImage(须含 TRANSFER_DST usage;首用即从 UNDEFINED 迁入传输布局)。
    pub image: vk::Image,
    /// 本批 staging 内的源偏移(4 的倍数——RGBA8 texel block 对齐,
    /// VUID-vkCmdCopyBufferToImage-pRegions-06223)。
    pub src_offset: u64,
    pub width: u32,
    pub height: u32,
}

/// 异族 release(图像):所有权从 transfer 族让渡。图像侧与 buffer 侧不同——
/// release 屏障**同时携带布局转换**(TRANSFER_DST → SHADER_READ_ONLY),
/// 一次屏障同时完成"迁出到可采样布局"与"让渡所有权"(sync.adoc 标准形状,
/// 比拆成两个屏障少一次全管线等待)。
#[derive(Clone, Copy, Debug)]
pub struct ImageRelease {
    pub image: vk::Image,
    /// 接收所有权的队列族(3.3.3 起 = graphics 族)。
    pub to_family: u32,
}

/// 一次 transfer 提交的全部内容。
#[derive(Default)]
pub struct UploadBatch {
    /// 本批合入 staging 的全部字节(从 0 连续排布,分段偏移由调用方记录)。
    pub staging: Vec<u8>,
    /// staging → 池的拷贝段(src 一律是本批 staging)。
    pub uploads: Vec<StagingCopy>,
    /// staging → 贴图的拷贝段(2D mip0 单层;每图配"迁入传输布局"前置屏障与
    /// "迁出为可采样"后置屏障,见录制段)。
    pub image_uploads: Vec<StagingImageCopy>,
    /// 池 → 池的设备侧拷贝(迁移;同提交内与 uploads 间以屏障收口)。
    pub device_copies: Vec<CopyRegion>,
    /// 批末尾的异族 release(同族回退时保持空)。
    pub releases: Vec<Release>,
    /// 图像侧异族 release:在场的图按"迁出布局+让渡"合成屏障;缺席的图按
    /// 纯布局转换收尾(同族回退/暂无消费者形状)。
    pub image_releases: Vec<ImageRelease>,
}

/// 一个复用槽位:staging 与命令缓冲成对,同槽同票据。
struct RingSlot {
    staging: Option<GpuBuffer>,
    command_buffer: vk::CommandBuffer,
    in_flight: Option<Ticket>,
}

/// transfer 队列上的合批上传器:staging 环 + timeline 票据 + release 挂点。
#[derive(bevy::prelude::Resource)]
pub struct Uploader {
    device: Device,
    queue_family: u32,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    slots: Vec<RingSlot>,
    cursor: usize,
    /// 完成票据的载体(timeline 信号量,初值 0)。
    ticket_semaphore: vk::Semaphore,
    /// 下一个要 signal 的票据值(从 1 起;0 专表空批,永不 signal)。
    next_ticket: Ticket,
    /// 新建 staging 槽的初始容量(按需再扩)。
    staging_capacity: u64,
    /// staging 扩容时按契约选型用(设备事实快照的克隆)。
    contract: MemoryContract,
}

impl Uploader {
    /// 建上传器:transfer 族命令池 + `slots` 个轮转槽(各含空 staging,按需
    /// 扩容)+ timeline 票据信号量。staging 初次只付 `staging_capacity`。
    ///
    /// # Errors
    /// 命令池/命令缓冲/信号量创建失败。
    pub fn new(
        device: &Device,
        contract: &MemoryContract,
        queue_family: u32,
        queue: vk::Queue,
        staging_capacity: u64,
        slots: usize,
    ) -> Result<Self, VulkanError> {
        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    // 槽位命令缓冲逐个重置重录,RESET_COMMAND_BUFFER 足够;族必须
                    // 是提交目标族(3.2.4.1:transfer 命令池归正确队列族)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(queue_family),
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
        let result = unsafe {
            Self::new_inner(
                device,
                contract,
                queue_family,
                queue,
                staging_capacity,
                command_pool,
                &command_buffers,
            )
        };
        if result.is_err() {
            unsafe { device.destroy_command_pool(command_pool, None) };
        }
        result
    }

    /// 创建链后半段(命令池已建;信号量失败时回收池)。
    ///
    /// # Safety
    /// `command_pool` 与 `command_buffers` 须刚创建成功且未销毁。
    unsafe fn new_inner(
        device: &Device,
        contract: &MemoryContract,
        queue_family: u32,
        queue: vk::Queue,
        staging_capacity: u64,
        command_pool: vk::CommandPool,
        command_buffers: &[vk::CommandBuffer],
    ) -> Result<Self, VulkanError> {
        // timeline:支持(1.3 核心 mandatory)≠ 启用——timelineSemaphore 已在
        // 设备创建时显式开启(context.rs);此处只用不另设
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
                .map(|&command_buffer| RingSlot {
                    staging: None,
                    command_buffer,
                    in_flight: None,
                })
                .collect(),
            cursor: 0,
            ticket_semaphore,
            next_ticket: 1,
            staging_capacity,
            contract: contract.clone(),
        })
    }

    /// 已发出的最高票据(下一批将 signal 这个值 +1)。
    #[must_use]
    pub fn last_issued_ticket(&self) -> Ticket {
        self.next_ticket - 1
    }

    /// 提交目标队列族(发布方判断"图像 release 是否需要让渡"的证据口)。
    #[must_use]
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }

    /// timeline 当前计数 = GPU 已完成的最高票据(证据查询口)。
    ///
    /// # Errors
    /// `vkGetSemaphoreCounterValue` 失败(设备丢失等)。
    pub fn completed_ticket(&self) -> Result<Ticket, VulkanError> {
        unsafe {
            self.device
                .get_semaphore_counter_value(self.ticket_semaphore)
        }
        .map_err(VulkanError::from)
    }

    /// 票据信号量本体:3.4 图形提交把它挂进 wait 列表(值 = 数据对应票据)实现
    /// "上传完成才可使用"的 GPU 侧等待;探针按同一形状做跨族预演。
    #[must_use]
    pub fn ticket_semaphore(&self) -> vk::Semaphore {
        self.ticket_semaphore
    }

    /// CPU 等到"截至 `ticket` 的全部上传批次"完成。合法调用点只有三处:
    /// 槽位复用、池维护、探针/验证——逐批路径没有它(判定线 6)。
    ///
    /// # Errors
    /// 等待超时(5s,合法合批远快于此;超时按设备异常冒泡)或调用失败。
    pub fn wait_until(&self, ticket: Ticket) -> Result<(), VulkanError> {
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

    /// 等全部已发出票据完成(池扩容迁移前的第一步)。
    ///
    /// # Errors
    /// 同 [`Self::wait_until`]。
    pub fn wait_all_uploads(&self) -> Result<(), VulkanError> {
        self.wait_until(self.last_issued_ticket())
    }

    /// 提交一批:staging 落盘(含非 coherent 自动 flush)→ 命令重置/录制
    /// (uploads / 设备侧拷贝 + 写读屏障 / 异族 release)→ 提交并 signal 新票据。
    /// 空批不提交;失败不发布票据(早退发生在 `next_ticket` 改写之前)。
    ///
    /// # Errors
    /// staging 扩容/写入、命令重置/录制/提交任一失败。
    pub fn submit_batch(&mut self, batch: UploadBatch) -> Result<Option<Ticket>, VulkanError> {
        let staging_len = batch.staging.len() as u64;
        if staging_len == 0
            && batch.uploads.is_empty()
            && batch.device_copies.is_empty()
            && batch.image_uploads.is_empty()
        {
            return Ok(None); // 空批次不提交,不留无人 signal 的票据
        }
        // 先算槽位与"要不要等"——票据等待发生在槽位可变借用之前(借用分离)
        let index = self.cursor;
        self.cursor = (self.cursor + 1) % self.slots.len();
        let pending = self.slots[index].in_flight;
        let completed = self.completed_ticket()?;
        if pending.is_some_and(|p| completed < p) {
            let p = pending.expect("is_some_and 已判定存在");
            self.wait_until(p)?;
            info!("staging 槽位 {index} 复用:等上批票据 {p} 完成后重录(按完成票据复用)");
        }
        // staging 按需整槽扩容(旧槽 staging 的 transfer 读已被票据等待保证完成)
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
        // 批内容落盘 staging:宿主写(非 coherent 时 write 内建 atom 对齐 flush)。
        // 这一步必须在录制/提交之前——拷贝的源是 staging 映射内存
        let staging_buffer = slot.staging.as_mut().expect("staging 已在上一步就绪");
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
                // 拷贝条款由调用方对齐纪律 + 角色 usage 前置满足:src/dst 偏移与
                // size 为 4 的倍数(00113/00114/00115)、src 含 TRANSFER_SRC、
                // dst 含 TRANSFER_DST(00118/00120)
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
                // 图像段(3.3.1):布局迁移与 buffer 读写次序是两类问题(显存机制篇
                // 判定线 3),独立录制,不套 buffer barrier 模板。
                //
                // 前置:UNDEFINED → TRANSFER_DST。首用自持——src/dst 族号用 IGNORED:
                // 3.4 定案后生产图像是 CONCURRENT 双族(显式族号会被 VUID 拒,必须
                // IGNORED),探针的 EXCLUSIVE 图像无让渡场景时 IGNORED 同样合法;
                // "把归谁写成代码"的显式族号只保留在 release 屏障(下方)。UNDEFINED
                // 起点无先前访问要罩,src 作用域为空(TOP_OF_PIPE 只满足非零前提);
                // 对比 D6 的进场屏障:那里 UNDEFINED 迁移前有 acquire 读要罩,srcStage
                // 必须提到 COLOR_ATTACHMENT_OUTPUT——两处形状不同是语义不同,不是模板。
                for image in distinct_images(&batch.image_uploads) {
                    let into_transfer = vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(color_mip0());
                    device.cmd_pipeline_barrier(
                        slot.command_buffer,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[into_transfer],
                    );
                }
                // 拷贝:staging → image,2D mip0 单层,紧 packing(行距驱动按格式推)。
                // bufferOffset 须为 texel block(4B)倍数——staging 分段偏移的调用方
                // 纪律(mesh 字节 32B 步长倍数、贴图字节 w*h*4,均过 4)。
                for region in &batch.image_uploads {
                    device.cmd_copy_buffer_to_image(
                        slot.command_buffer,
                        staging_buffer.buffer(),
                        region.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy {
                            buffer_offset: region.src_offset,
                            buffer_row_length: 0,
                            buffer_image_height: 0,
                            image_subresource: vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .mip_level(0)
                                .base_array_layer(0)
                                .layer_count(1),
                            image_offset: vk::Offset3D::default(),
                            image_extent: vk::Extent3D {
                                width: region.width,
                                height: region.height,
                                depth: 1,
                            },
                        }],
                    );
                }
                // 后置:TRANSFER_DST → SHADER_READ_ONLY。src 作用域罩住本提交内的
                // 拷贝写(TRANSFER_WRITE);本提交内没有消费者,dst 作用域为空——真正的
                // 读依赖由消费方"等票据 + 自己的屏障"建立(票据信号量完成 memory
                // 释放/获取语义)。带 release 的图把所有权让渡合成进同一条屏障,
                // dst 阶段/访问掩码被规范声明忽略(与 buffer release 同款);不带
                // release 的图按同族纯布局转换收尾(IGNORED = 无所有权语义)。
                for image in distinct_images(&batch.image_uploads) {
                    let release_to = batch
                        .image_releases
                        .iter()
                        .find(|r| r.image == image)
                        .map(|r| r.to_family);
                    let out_of_transfer = match release_to {
                        Some(to_family) => vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                            .src_queue_family_index(queue_family)
                            .dst_queue_family_index(to_family),
                        None => vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED),
                    }
                    .image(image)
                    .subresource_range(color_mip0());
                    device.cmd_pipeline_barrier(
                        slot.command_buffer,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[out_of_transfer],
                    );
                }
            }
            if !batch.device_copies.is_empty() {
                // 同提交内的写后读(回读/迁移读池):TRANSFER_WRITE → TRANSFER_READ
                // 逐池收口(barrier 必须挂具体 buffer,null 非法)
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
                // release:dst 阶段/访问掩码被规范声明为"忽略"(ownership transfer),
                // 填 BOTTOM_OF_PIPE/空只满足旧式屏障掩码非零前提;配对 acquire 在
                // 图形族提交侧(全链形状见探针跨族组,3.4 按此接线)
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

            // VUID-03239:timeline 进 signal 列表就必须挂 TimelineSemaphoreSubmitInfo;
            // VUID-03241:signalSemaphoreValueCount == signalSemaphoreCount(都是 1)
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
        // 提交已成功:占槽、发新票据。上方任何 ? 早退都到不了这里——
        // 虚假完成票据不存在(3.2.4.1/3.2.4.4)
        slot.in_flight = Some(ticket);
        self.next_ticket += 1;
        Ok(Some(ticket))
    }
}

/// device_copies 里出现过的不同 src buffer(写后读屏障逐 buffer 挂)。
fn distinct_srcs(regions: &[CopyRegion]) -> Vec<vk::Buffer> {
    let mut seen = Vec::new();
    for region in regions {
        if !seen.contains(&region.src) {
            seen.push(region.src);
        }
    }
    seen
}

/// image_uploads 里出现过的不同目标 image(前/后置屏障逐图挂)。
fn distinct_images(regions: &[StagingImageCopy]) -> Vec<vk::Image> {
    let mut seen = Vec::new();
    for region in regions {
        if !seen.contains(&region.image) {
            seen.push(region.image);
        }
    }
    seen
}

/// 2D mip0 单层的 COLOR 子资源范围(图像屏障的统一口径,与 view 创建一致)。
fn color_mip0() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

impl Drop for Uploader {
    fn drop(&mut self) {
        // 契约边界(与 GpuBuffer/MeshPool 同款):不等 GPU;退出排空由
        // teardown_vulkan 的 device_wait_idle 先于一切资源 Drop(D4 定案)。
        // staging 随槽位一起 Drop。
        unsafe {
            self.device.destroy_semaphore(self.ticket_semaphore, None);
            self.device.destroy_command_pool(self.command_pool, None);
        }
    }
}
