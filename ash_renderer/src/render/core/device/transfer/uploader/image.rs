//! 图像上传、布局转换和 graphics 队列 mip 生成。

use std::slice;

use ash::{vk, Device};

use crate::common::error::Result;

use super::super::types::{StagingImageCopy, Ticket, UploadBatch};
use super::Uploader;

pub(super) fn record_image_uploads(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    queue_family: u32,
    staging_buffer: vk::Buffer,
    batch: &UploadBatch,
) {
    unsafe {
        // 首次使用的图像从 UNDEFINED 开始，没有旧访问需要等待。
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
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[into_transfer],
            );
        }
        // 从紧密排列的 staging 数据拷贝 mip0。
        for region in &batch.image_uploads {
            device.cmd_copy_buffer_to_image(
                command_buffer,
                staging_buffer,
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
        // 多级图像将 mip0 转为 TRANSFER_SRC，供 graphics 续批读取。
        let mut chain_seen: Vec<vk::Image> = Vec::new();
        for region in &batch.image_uploads {
            if region.mip_levels <= 1 || chain_seen.contains(&region.image) {
                continue;
            }
            chain_seen.push(region.image);
            // 转换写入对后续 blit 的 TRANSFER_READ 可见；跨提交依赖由票据衔接。
            let src_ready = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(region.image)
                .subresource_range(color_mip0());
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
        for image in distinct_images(&batch.image_uploads) {
            let is_chained = batch
                .image_uploads
                .iter()
                .any(|r| r.image == image && r.mip_levels > 1);
            if is_chained {
                continue;
            }
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

pub(super) fn chained_images(regions: &[StagingImageCopy]) -> Vec<&StagingImageCopy> {
    let mut out: Vec<&StagingImageCopy> = Vec::new();
    for region in regions {
        if region.mip_levels > 1 && !out.iter().any(|r| r.image == region.image) {
            out.push(region);
        }
    }
    out
}

fn distinct_images(regions: &[StagingImageCopy]) -> Vec<vk::Image> {
    let mut seen = Vec::new();
    for region in regions {
        if !seen.contains(&region.image) {
            seen.push(region.image);
        }
    }
    seen
}

fn color_mip0() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
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
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .base_mip_level(1)
                        .level_count(region.mip_levels - 1)
                        .layer_count(1),
                );
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
                // Halve each dimension per level, clamping both to one pixel.
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
                        .src_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .mip_level(level - 1)
                                .base_array_layer(0)
                                .layer_count(1),
                        )
                        .src_offsets([
                            vk::Offset3D::default(),
                            vk::Offset3D {
                                x: src_w as i32,
                                y: src_h as i32,
                                z: 1,
                            },
                        ])
                        .dst_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .mip_level(level)
                                .base_array_layer(0)
                                .layer_count(1),
                        )
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
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_mip_level(level)
                            .level_count(1)
                            .layer_count(1),
                    );
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
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(region.mip_levels)
                        .layer_count(1),
                );
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
