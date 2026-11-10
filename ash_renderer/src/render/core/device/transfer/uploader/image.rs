//! 图像上传、布局转换和 graphics 队列 mip 生成。

use std::slice;

use ash::{vk, Device};

use crate::common::error::Result;

use super::super::types::{
    is_block_compressed, level_bytes, StagingImageCopy, Ticket, UploadBatch,
};
use super::{distinct, Uploader};

pub(super) fn record_image_uploads(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    queue_family: u32,
    staging_buffer: vk::Buffer,
    batch: &UploadBatch,
) {
    unsafe {
        // 首次使用的图像从 UNDEFINED 开始，没有旧访问需要等待。
        // 块压缩图 mip 全链一次进写布局（各级都在本段拷入）；未压缩只动 mip0。
        for image in distinct_images(&batch.image_uploads) {
            let mip_count = batch
                .image_uploads
                .iter()
                .find(|r| r.image == image)
                .map_or(1, |r| {
                    if is_block_compressed(r.format) {
                        r.mip_levels
                    } else {
                        1
                    }
                });
            let into_transfer = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_mips(0, mip_count));
            device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[into_transfer],
            );
        }
        for region in &batch.image_uploads {
            if is_block_compressed(region.format) {
                // 块压缩：全链逐级拷入，块算术换算（块字节数倍数的 bufferOffset 红线
                // 由排批侧对齐保证）。bufferRowLength/Height 写 0 表示紧排。
                let mut offset = region.src_offset;
                for level in 0..region.mip_levels {
                    let (lw, lh) = level_extent(region, level);
                    device.cmd_copy_buffer_to_image(
                        command_buffer,
                        staging_buffer,
                        region.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy {
                            buffer_offset: offset,
                            buffer_row_length: 0,
                            buffer_image_height: 0,
                            image_subresource: color_layer(level),
                            image_offset: vk::Offset3D::default(),
                            image_extent: vk::Extent3D {
                                width: lw,
                                height: lh,
                                depth: 1,
                            },
                        }],
                    );
                    offset += level_bytes(region.format, lw, lh);
                }
                continue;
            }
            // 从紧密排列的 staging 数据拷贝 mip0。
            device.cmd_copy_buffer_to_image(
                command_buffer,
                staging_buffer,
                region.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy {
                    buffer_offset: region.src_offset,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: color_layer(0),
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D {
                        width: region.width,
                        height: region.height,
                        depth: 1,
                    },
                }],
            );
        }
        // 多级未压缩图像将 mip0 转为 TRANSFER_SRC，供 graphics 续批读取。
        let chained = chained_images(&batch.image_uploads);
        for region in &chained {
            // 转换写入对后续 blit 的 TRANSFER_READ 可见；跨提交依赖由票据衔接。
            let src_ready = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(region.image)
                .subresource_range(color_mips(0, 1));
            device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[src_ready],
            );
        }
        // 该屏障覆盖拷贝写入；消费方以票据等待并建立 shader 读屏障。
        // 块压缩图收尾覆盖全链（各级都在本段写入）。
        for image in distinct_images(&batch.image_uploads) {
            if chained.iter().any(|r| r.image == image) {
                continue;
            }
            let mip_count = batch
                .image_uploads
                .iter()
                .find(|r| r.image == image)
                .map_or(1, |r| {
                    if is_block_compressed(r.format) {
                        r.mip_levels
                    } else {
                        1
                    }
                });
            // 有 release 时顺带把所有权交给目标队列族
            let (src_family, dst_family) = batch
                .image_releases
                .iter()
                .find(|r| r.image == image)
                .map_or((vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED), |r| {
                    (queue_family, r.to_family)
                });
            let out_of_transfer = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(src_family)
                .dst_queue_family_index(dst_family)
                .image(image)
                .subresource_range(color_mips(0, mip_count));
            device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[out_of_transfer],
            );
        }
    }
}

/// 某 mip 级的像素尺寸（最小 1，末级合法）。
fn level_extent(region: &StagingImageCopy, level: u32) -> (u32, u32) {
    (
        (region.width >> level).max(1),
        (region.height >> level).max(1),
    )
}

pub(super) fn chained_images(regions: &[StagingImageCopy]) -> Vec<&StagingImageCopy> {
    let mut out: Vec<&StagingImageCopy> = Vec::new();
    for region in regions {
        // 块压缩图不进 blit 续批（blit 不支持压缩格式，mip 链已预生成）
        if region.mip_levels > 1
            && !is_block_compressed(region.format)
            && !out.iter().any(|r| r.image == region.image)
        {
            out.push(region);
        }
    }
    out
}

fn distinct_images(regions: &[StagingImageCopy]) -> Vec<vk::Image> {
    distinct(regions.iter().map(|r| r.image))
}

fn color_mips(base_mip: u32, mip_count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(base_mip)
        .level_count(mip_count)
        .layer_count(1)
}

fn color_layer(mip: u32) -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(mip)
        .base_array_layer(0)
        .layer_count(1)
}

pub(super) fn record_blit_continuation(
    uploader: &Uploader,
    blit_cb: vk::CommandBuffer,
    transfer_ticket: Ticket,
    chained: &[&StagingImageCopy],
) -> Result<()> {
    let published = transfer_ticket + 1;
    unsafe {
        uploader
            .device
            .reset_command_buffer(blit_cb, vk::CommandBufferResetFlags::empty())?;
        uploader.device.begin_command_buffer(
            blit_cb,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        for region in chained {
            // 将高层 mip 迁入写布局；mip0 已作为 blit 源。
            let dst_ready = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(region.image)
                .subresource_range(color_mips(1, region.mip_levels - 1));
            uploader.device.cmd_pipeline_barrier(
                blit_cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[dst_ready],
            );
            for level in 1..region.mip_levels {
                let (src_w, src_h) = (
                    (region.width >> (level - 1)).max(1),
                    (region.height >> (level - 1)).max(1),
                );
                let (dst_w, dst_h) = (
                    (region.width >> level).max(1),
                    (region.height >> level).max(1),
                );
                uploader.device.cmd_blit_image(
                    blit_cb,
                    region.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    region.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::ImageBlit::default()
                        .src_subresource(color_layer(level - 1))
                        .src_offsets([
                            vk::Offset3D::default(),
                            vk::Offset3D {
                                x: src_w as i32,
                                y: src_h as i32,
                                z: 1,
                            },
                        ])
                        .dst_subresource(color_layer(level))
                        .dst_offsets([
                            vk::Offset3D::default(),
                            vk::Offset3D {
                                x: dst_w as i32,
                                y: dst_h as i32,
                                z: 1,
                            },
                        ])],
                    vk::Filter::LINEAR,
                );
                // 下一级 blit 会读取此 mip；屏障覆盖其布局转换写入。
                let level_ready = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(region.image)
                    .subresource_range(color_mips(level, 1));
                uploader.device.cmd_pipeline_barrier(
                    blit_cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[level_ready],
                );
            }
            // 覆盖各级 blit 写入，并将完整 mip 链交给 shader 读取。
            let finalize = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(region.image)
                .subresource_range(color_mips(0, region.mip_levels));
            uploader.device.cmd_pipeline_barrier(
                blit_cb,
                vk::PipelineStageFlags::TRANSFER,
                // SHADER_READ 的消费者是片元着色器。
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[finalize],
            );
        }
        uploader.device.end_command_buffer(blit_cb)?;
        // 等待 transfer 票据并 signal 续批完成票据。
        let mut timeline_submit = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(slice::from_ref(&transfer_ticket))
            .signal_semaphore_values(slice::from_ref(&published));
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let cbs = slice::from_ref(&blit_cb);
        let sem = slice::from_ref(&uploader.ticket_semaphore);
        uploader.device.queue_submit(
            uploader.graphics_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(cbs)
                .wait_semaphores(sem)
                .wait_dst_stage_mask(&wait_stages)
                .signal_semaphores(sem)
                .push_next(&mut timeline_submit)],
            vk::Fence::null(),
        )?;
    }
    Ok(())
}
