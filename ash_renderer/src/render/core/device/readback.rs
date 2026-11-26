//! 诊断回读设施：一次性 staging + 命令池 + fence 的同步回读。低频 BRP 读数
//! 方法族（remote/）与 render 内部（Hi-Z 级读回、透明列表回读）共用；不在帧
//! 循环内调用——每次调用独占一次提交并阻塞到 fence。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};
use crate::render::core::record::barrier::{ImageBarrier, MemoryBarrierDesc};

use super::context::Context;

/// 把一段 buffer 拷进一次性 staging 并回读到 `out`（拷 `out.len()` 字节）。
///
/// # Errors
/// staging 创建/提交/等待失败（设备丢失等）。
pub(crate) fn readback_buffer(ctx: &Context, src: vk::Buffer, out: &mut [u8]) -> Result<()> {
    // 零长度是合法的诊断输入（对象槽高水位/在场列表为空）：0 字节 buffer 与 0 字节
    // 拷贝都违反规范（VUID-VkBufferCreateInfo-size-00912 / VkBufferCopy-size-01988），直接当成功。
    if out.is_empty() {
        return Ok(());
    }
    let byte_size = out.len() as u64;
    unsafe {
        let device = &ctx.device;
        let readback = OneShotReadback::begin(ctx, byte_size)?;
        // 源 buffer 的写者在前序提交（compute 写标记/索引、transfer 上传等）：
        // 全局内存屏障按提交序把它们纳入第一同步域，拷贝读才见得到写入
        MemoryBarrierDesc {
            src_stage: vk::PipelineStageFlags::ALL_COMMANDS,
            src_access: vk::AccessFlags::MEMORY_WRITE,
            dst_stage: vk::PipelineStageFlags::TRANSFER,
            dst_access: vk::AccessFlags::TRANSFER_READ,
        }
        .record(device, readback.cb);
        let region = vk::BufferCopy::default().size(byte_size);
        device.cmd_copy_buffer(
            readback.cb,
            src,
            readback.staging,
            std::slice::from_ref(&region),
        );
        readback.finish_and_read(ctx, out)
    }
}

/// 回读未压缩 2D 图像的 mip0/layer0 区域，提交结束前恢复原布局。
/// 调用方须保证区域位于图像内、字节数符合格式，且图像具备 TRANSFER_SRC 用途。
///
/// # Errors
/// 区域为空/长度溢出，或 staging 创建/提交/等待/映射失败。
pub(crate) fn readback_image_region(
    ctx: &Context,
    image: vk::Image,
    aspect: vk::ImageAspectFlags,
    layout: vk::ImageLayout,
    offset: vk::Offset2D,
    extent: vk::Extent2D,
    bytes_per_texel: u64,
) -> Result<Vec<u8>> {
    if offset.x < 0 || offset.y < 0 {
        return Err(VulkanError::Init("图像回读区域偏移不得为负".to_owned()));
    }
    let len = image_region_len(extent, bytes_per_texel)?;
    let mut bytes = vec![0u8; len];
    unsafe {
        let readback = OneShotReadback::begin(ctx, len as u64)?;
        ImageBarrier::transition(
            image,
            aspect,
            (layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
            (
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
            ),
            (
                vk::AccessFlags::MEMORY_WRITE,
                vk::AccessFlags::TRANSFER_READ,
            ),
        )
        .record(&ctx.device, readback.cb);
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(aspect)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: offset.x,
                y: offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        ctx.device.cmd_copy_image_to_buffer(
            readback.cb,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.staging,
            std::slice::from_ref(&region),
        );
        ImageBarrier::transition(
            image,
            aspect,
            (vk::ImageLayout::TRANSFER_SRC_OPTIMAL, layout),
            (
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
            ),
            (
                vk::AccessFlags::TRANSFER_READ,
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            ),
        )
        .record(&ctx.device, readback.cb);
        readback.finish_and_read(ctx, &mut bytes)?;
    }
    Ok(bytes)
}

fn image_region_len(extent: vk::Extent2D, bytes_per_texel: u64) -> Result<usize> {
    if extent.width == 0 || extent.height == 0 || bytes_per_texel == 0 {
        return Err(VulkanError::Init(
            "图像回读区域与像素字节数不得为零".to_owned(),
        ));
    }
    u64::from(extent.width)
        .checked_mul(u64::from(extent.height))
        .and_then(|n| n.checked_mul(bytes_per_texel))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| VulkanError::Init("图像回读区域字节数溢出".to_owned()))
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
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::TRANSFER,
                src_access: vk::AccessFlags::TRANSFER_WRITE,
                dst_stage: vk::PipelineStageFlags::HOST,
                dst_access: vk::AccessFlags::HOST_READ,
            }
            .record(device, self.cb);
            device.end_command_buffer(self.cb)?;
            device.queue_submit(
                ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cb))],
                self.fence,
            )?;
            device.wait_for_fences(std::slice::from_ref(&self.fence), true, u64::MAX)?;
            let ptr =
                device.map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_region_size_is_tightly_packed_and_checked() {
        assert_eq!(
            image_region_len(
                vk::Extent2D {
                    width: 3,
                    height: 3
                },
                4
            )
            .unwrap(),
            36
        );
        assert_eq!(
            image_region_len(
                vk::Extent2D {
                    width: 1,
                    height: 1
                },
                8
            )
            .unwrap(),
            8
        );
        assert!(image_region_len(
            vk::Extent2D {
                width: 0,
                height: 3
            },
            4
        )
        .is_err());
        assert!(image_region_len(
            vk::Extent2D {
                width: 3,
                height: 3
            },
            0
        )
        .is_err());
        assert!(image_region_len(
            vk::Extent2D {
                width: u32::MAX,
                height: u32::MAX
            },
            8
        )
        .is_err());
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
