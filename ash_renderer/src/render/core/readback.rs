//! 诊断回读设施：一次性 staging + 命令池 + fence 的同步回读。低频 BRP 读数
//! 方法族（remote/）与 render 内部（Hi-Z 级读回、透明列表回读）共用；不在帧
//! 循环内调用——每次调用独占一次提交并阻塞到 fence。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::Context;

/// 把一段 buffer 拷进一次性 staging 并回读到 `out`（拷 `out.len()` 字节）。
///
/// # Errors
/// staging 创建/提交/等待失败（设备丢失等）。
pub(crate) fn readback_buffer(ctx: &Context, src: vk::Buffer, out: &mut [u8]) -> Result<()> {
    let byte_size = out.len() as u64;
    unsafe {
        let device = &ctx.device;
        let readback = OneShotReadback::begin(ctx, byte_size)?;
        // 源 buffer 的写者在前序提交（compute 写标记/索引、transfer 上传等）：
        // 全局内存屏障按提交序把它们纳入第一同步域，拷贝读才见得到写入
        let before_copy = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        device.cmd_pipeline_barrier(
            readback.cb,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            std::slice::from_ref(&before_copy),
            &[],
            &[],
        );
        let region = vk::BufferCopy::default().size(byte_size);
        device.cmd_copy_buffer(readback.cb, src, readback.staging, std::slice::from_ref(&region));
        readback.finish_and_read(ctx, out)
    }
}

/// 一次性诊断回读资源：staging（HOST_VISIBLE，HOST_COHERENT 优先）+ 命令池 +
/// 单命令缓冲 + fence。调用方在 [`Self::begin`] 与 [`Self::finish_and_read`]
/// 之间往 `cb` 录"拷进 `staging`"的命令。Drop 统一回收，成败路径同一出口；
/// 未建成的句柄保持 null（destroy/free null 句柄是规范允许的 no-op）。Drop 不
/// 等 GPU：正常路径已等完 fence，提交/等待失败属设备丢失类，不再有可推进的
/// 在飞引用。
pub(crate) struct OneShotReadback<'a> {
    device: &'a Device,
    pub(crate) staging: vk::Buffer,
    memory: vk::DeviceMemory,
    pool: vk::CommandPool,
    pub(crate) cb: vk::CommandBuffer,
    fence: vk::Fence,
}

impl<'a> OneShotReadback<'a> {
    /// 建 staging + 命令池 + fence，并以 ONE_TIME_SUBMIT 开录 `cb`。
    ///
    /// # Safety
    /// `ctx.device` 合法且寿命覆盖返回值。
    pub(crate) unsafe fn begin(ctx: &'a Context, byte_size: u64) -> Result<Self> {
        let device = &ctx.device;
        let mut this = Self {
            device,
            staging: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            pool: vk::CommandPool::null(),
            cb: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
        };
        unsafe {
            this.staging = device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(byte_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )?;
            let reqs = device.get_buffer_memory_requirements(this.staging);
            let memory_type = ctx.memory_contract().find_type(
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            this.memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            device.bind_buffer_memory(this.staging, this.memory, 0)?;
            this.pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family_index),
                None,
            )?;
            this.cb = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(this.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            this.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            device.begin_command_buffer(
                this.cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        Ok(this)
    }

    /// 收尾录制 → 提交 → 等 fence → 映射拷出 `out.len()` 字节。录制尾部补
    /// TRANSFER_WRITE → HOST_READ 屏障（fence 等待本身不让设备写对 host 可
    /// 见）；映射后 invalidate（staging 落到非 HOST_COHERENT 类型时必需，
    /// coherent 内存上是 no-op）。
    ///
    /// # Safety
    /// `cb` 已由 [`Self::begin`] 开录，录入命令引用的资源在等待返回前有效。
    pub(crate) unsafe fn finish_and_read(&self, ctx: &Context, out: &mut [u8]) -> Result<()> {
        let device = self.device;
        unsafe {
            let to_host = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ);
            device.cmd_pipeline_barrier(
                self.cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                std::slice::from_ref(&to_host),
                &[],
                &[],
            );
            device.end_command_buffer(self.cb)?;
            device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cb))],
                self.fence,
            )?;
            device.wait_for_fences(std::slice::from_ref(&self.fence), true, u64::MAX)?;
            // ash 0.38 形状：map 返回 NonNull 指针
            let ptr = device
                .map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(VulkanError::from)?;
            let range = vk::MappedMemoryRange::default()
                .memory(self.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            let invalidated = device.invalidate_mapped_memory_ranges(std::slice::from_ref(&range));
            if invalidated.is_ok() {
                out.copy_from_slice(std::slice::from_raw_parts(ptr.cast::<u8>(), out.len()));
            }
            device.unmap_memory(self.memory);
            invalidated.map_err(VulkanError::from)
        }
    }
}

impl Drop for OneShotReadback<'_> {
    fn drop(&mut self) {
        // 逆创建序拆；命令缓冲随命令池回收
        unsafe {
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_buffer(self.staging, None);
            self.device.free_memory(self.memory, None);
        }
    }
}
