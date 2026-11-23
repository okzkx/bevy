//! 屏障录制助手：图像布局转换与全局内存依赖。用哪个阶段、访问、布局由调用方传入，屏障为什么这样设的说明写在调用处。
//!
//! 所有 `unsafe fn` 的前提与 ash 的 `cmd_*` 相同：`cb` 处于录制状态，传入的句柄合法且活到命令执行完。

use std::slice;

use ash::{vk, Device};

/// 一条图像布局屏障，默认只作用于第 0 层 mip、第 0 层 layer（[`Self::with_range`] 可扩到
/// 多 mip/多层的子资源范围，cube 的全子资源迁移就要它）。
/// 每条单独录成一次 `vkCmdPipelineBarrier`，阶段掩码就用本条的 src/dst。
#[derive(Clone, Copy)]
pub(crate) struct ImageBarrier {
    pub image: vk::Image,
    pub aspect: vk::ImageAspectFlags,
    pub old_layout: vk::ImageLayout,
    pub new_layout: vk::ImageLayout,
    pub src_stage: vk::PipelineStageFlags,
    pub src_access: vk::AccessFlags,
    pub dst_stage: vk::PipelineStageFlags,
    pub dst_access: vk::AccessFlags,
    pub base_mip: u32,
    pub level_count: u32,
    pub base_layer: u32,
    pub layer_count: u32,
}

impl ImageBarrier {
    /// 通用布局迁移：四元组由调用方给（阶段/访问成对，布局成对），子资源范围取默认
    /// （mip0 / layer0），要跨 mip/层再用 [`Self::with_range`] 覆盖。
    pub(crate) fn transition(
        image: vk::Image,
        aspect: vk::ImageAspectFlags,
        layouts: (vk::ImageLayout, vk::ImageLayout),
        stages: (vk::PipelineStageFlags, vk::PipelineStageFlags),
        access: (vk::AccessFlags, vk::AccessFlags),
    ) -> Self {
        Self {
            image,
            aspect,
            old_layout: layouts.0,
            new_layout: layouts.1,
            src_stage: stages.0,
            src_access: access.0,
            dst_stage: stages.1,
            dst_access: access.1,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 覆盖子资源范围（cube 的全子资源 = `(0, mips, 0, 6)`）。
    pub(crate) fn with_range(
        mut self,
        base_mip: u32,
        level_count: u32,
        base_layer: u32,
        layer_count: u32,
    ) -> Self {
        self.base_mip = base_mip;
        self.level_count = level_count;
        self.base_layer = base_layer;
        self.layer_count = layer_count;
        self
    }

    /// 丢弃旧内容、转成颜色附件准备写入。只等 `src_stage` 执行完，不需要让旧内容可见。
    pub(crate) fn discard_to_color(image: vk::Image, src_stage: vk::PipelineStageFlags) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::UNDEFINED,
            new_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            src_stage,
            src_access: vk::AccessFlags::empty(),
            dst_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            dst_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 颜色附件写完，转成片元着色器可采样。
    pub(crate) fn color_to_sampled(image: vk::Image) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::COLOR,
            old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            src_stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            src_access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            dst_access: vk::AccessFlags::SHADER_READ,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 丢弃旧内容、转成深度附件准备写入。`src_stage` 由调用方给：本帧槽深度等上轮
    /// fence 耗尽取 EARLY|LATE_FRAGMENT_TESTS，跨帧共享的 shadow 图取 FRAGMENT_SHADER（WAR）。
    pub(crate) fn discard_to_depth_attachment(
        image: vk::Image,
        src_stage: vk::PipelineStageFlags,
    ) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::DEPTH,
            old_layout: vk::ImageLayout::UNDEFINED,
            new_layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            src_stage,
            src_access: vk::AccessFlags::empty(),
            dst_stage: vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            dst_access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 深度附件写完，转成片元着色器可采样（SHADER_READ_ONLY）。
    pub(crate) fn depth_attachment_to_sampled(image: vk::Image) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::DEPTH,
            old_layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            src_stage: vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            src_access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            dst_access: vk::AccessFlags::SHADER_READ,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 深度附件转只读（DEPTH_READ_ONLY），供片元（合成采样）与 compute（Hi-Z 归约）读。
    pub(crate) fn depth_attachment_to_read_only(image: vk::Image) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::DEPTH,
            old_layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            new_layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
            src_stage: vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            src_access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::COMPUTE_SHADER,
            dst_access: vk::AccessFlags::SHADER_READ,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 深度只读转回附件（透明层 LOAD 测深）。src 含 SHADER_READ（合成采样读过）与
    /// COLOR_ATTACHMENT_OUTPUT（同一次屏障还排合成写 HDR 的 memory barrier）。
    pub(crate) fn depth_read_only_to_attachment(image: vk::Image) -> Self {
        Self {
            image,
            aspect: vk::ImageAspectFlags::DEPTH,
            old_layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
            new_layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            src_stage: vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS
                | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            src_access: vk::AccessFlags::SHADER_READ
                | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            dst_stage: vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS
                | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            dst_access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ,
            base_mip: 0,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
        }
    }

    /// 录一条图像屏障。
    ///
    /// # Safety
    /// 见模块注释。
    pub(crate) unsafe fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        unsafe { self.record_with_memory(device, cb, &[]) };
    }

    /// 图像屏障和全局内存依赖合成一次 `vkCmdPipelineBarrier` 录出。
    /// 坑：阶段掩码只取本条的 src/dst，`memory` 只取 access 掩码——阶段覆盖由本条屏障负责。
    ///
    /// # Safety
    /// 见模块注释。
    pub(crate) unsafe fn record_with_memory(
        &self,
        device: &Device,
        cb: vk::CommandBuffer,
        memory: &[MemoryBarrierDesc],
    ) {
        let memory: std::vec::Vec<vk::MemoryBarrier> =
            memory.iter().map(|d| d.vk_barrier()).collect();
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(self.old_layout)
            .new_layout(self.new_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(self.aspect)
                    .base_mip_level(self.base_mip)
                    .level_count(self.level_count)
                    .base_array_layer(self.base_layer)
                    .layer_count(self.layer_count),
            )
            .src_access_mask(self.src_access)
            .dst_access_mask(self.dst_access);
        unsafe {
            device.cmd_pipeline_barrier(
                cb,
                self.src_stage,
                self.dst_stage,
                vk::DependencyFlags::empty(),
                &memory,
                &[],
                slice::from_ref(&barrier),
            );
        }
    }
}

/// 一条全局内存依赖屏障（`vk::MemoryBarrier`）：不迁移布局、不指定资源，纯保证
/// "src 的写在 dst 的读前可见"。buffer 没有 layout，填缓冲后的可见性依赖走这条。
/// 每条单独录成一次 `vkCmdPipelineBarrier`。
#[derive(Clone, Copy)]
pub(crate) struct MemoryBarrierDesc {
    pub src_stage: vk::PipelineStageFlags,
    pub src_access: vk::AccessFlags,
    pub dst_stage: vk::PipelineStageFlags,
    pub dst_access: vk::AccessFlags,
}

impl MemoryBarrierDesc {
    /// 借给图像屏障合成录制用：只取 access 掩码，阶段由合成的图像屏障提供。
    fn vk_barrier(self) -> vk::MemoryBarrier<'static> {
        vk::MemoryBarrier::default()
            .src_access_mask(self.src_access)
            .dst_access_mask(self.dst_access)
    }

    /// # Safety
    /// 见模块注释。
    pub(crate) unsafe fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        let barrier = self.vk_barrier();
        unsafe {
            device.cmd_pipeline_barrier(
                cb,
                self.src_stage,
                self.dst_stage,
                vk::DependencyFlags::empty(),
                slice::from_ref(&barrier),
                &[],
                &[],
            );
        }
    }
}
