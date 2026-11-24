//! 9.7.4 1/4 屏幕查表的录制束与目标（`scgi_l0` / `scgi_l1`）。
//!
//! 目标两张、各双缓冲（去噪要"读 A 写 B"，9.7.5）：`l0` = RGBA16F 存 L0 辐照度（物理单位）
//! 加覆盖率 a；`l1` = RGBA8 存三轴方向比值。
//!
//! 两张图以 `initialLayout = UNDEFINED` 创建（`vkCreateImage` 只接受 UNDEFINED 或
//! PREINITIALIZED——这条先被现场核验抓出来了），首次使用时各录一道 UNDEFINED→GENERAL 的
//! 屏障，之后一直留在 GENERAL：本段与后续 compute 都按 GENERAL 访问，省掉每帧的布局迁移
//! （内容每帧整体重写）。
//!
//! 目标随 swapchain 尺寸重建：**布局与描述符池只建一次**（`new`），重建只换 image/view 并
//! 重写同一张描述符集（2026-11-24 G-buffer 布局悬空的教训：布局进进程级，resize 只重建附件）。

use std::sync::atomic::{AtomicBool, Ordering};

use ash::{vk, Device};

use crate::common::bytes::put_u32s;
use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{
    BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc,
};
use crate::render::core::record::compute::{ComputeHandles, Handoff};
use crate::render::Context;

const CS: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// 1/4 L0 目标格式：物理单位辐照度 + 覆盖率（a）。
pub const SCGI_L0_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// 1/4 L1 目标格式：三轴方向比值编码 + 备用通道。
pub const SCGI_L1_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// 每张中间目标的双缓冲份数（A = 查表写、B = 去噪写；9.7.5 起轮换）。
pub const SCGI_BUFFERS: usize = 2;
/// `scgi_l0` 每像素字节数（RGBA16F）。
pub const SCGI_L0_BYTES: u64 = 16;
/// `scgi_l1` 每像素字节数（RGBA8）。
pub const SCGI_L1_BYTES: u64 = 4;
/// 目标图总数（两格式 × 双缓冲 + 全分辨率一张）：`ensure` 里一次性迁到 GENERAL 的那批。
pub const SCGI_IMAGES: usize = SCGI_BUFFERS * 2 + 1;

const _: () = assert!(SCGI_IMAGES == 5);

/// 全分辨率辐照度目标格式：rgb = 按全分辨率法线求值的 E(n)，a = 覆盖率。
pub const SCGI_IRRADIANCE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// `scgi_irradiance` 每像素字节数。
pub const SCGI_IRRADIANCE_BYTES: u64 = 16;

/// 目标集：b0/b1 = 主组（P0）的 l0/l1，b2/b3 = 副组（P1），b4 = 全分辨率辐照度。
///
/// 两个方向各一份集（[`ScgiTargets::set`] / [`ScgiTargets::set_reverse`]）：去噪要"读一组写
/// 另一组"，把方向写进描述符集比在 shader 里做分支简单，也不会出现"同 dispatch 边写边读"。
pub(crate) static SCGI_TARGET_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "scgi_targets",
    &[
        BindingDesc::new(0, DescKind::StorageImage, CS),
        BindingDesc::new(1, DescKind::StorageImage, CS),
        BindingDesc::new(2, DescKind::StorageImage, CS),
        BindingDesc::new(3, DescKind::StorageImage, CS),
        BindingDesc::new(4, DescKind::StorageImage, CS),
    ],
);

/// 合成 pass 侧的单张采样集：b0 = `scgi_irradiance`（片元读，`textureLoad`）。
///
/// 与 [`SCGI_TARGET_SET_DESC`] 分开是必须的：那个集是 compute 阶段的 storage image，
/// 片元阶段既不能写存储图、也不能混绑；合成只要"按像素取一张全分辨率辐照度"。
pub(crate) static SCGI_SAMPLE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "scgi_sample",
    &[BindingDesc::new(
        0,
        DescKind::SampledImage,
        vk::ShaderStageFlags::FRAGMENT,
    )],
);

/// 查表 push 字节：low_w / low_h / full_w / full_h / frame_index / normal_source / samples / 保留。
pub const GI_SCREEN_GATHER_PUSH_SIZE: u32 = 32;

/// 去噪 push 字节：low_w / low_h / full_w / full_h / step / 保留。
pub const GI_DENOISE_PUSH_SIZE: u32 = 24;

/// 上采样 push 字节：low_w / low_h / full_w / full_h / mode / 保留。
pub const GI_UPSAMPLE_PUSH_SIZE: u32 = 24;

/// 1/4 目标去噪的录制输入。
#[derive(Clone, Copy)]
pub struct GiDenoiseDraw {
    pub pass: ComputeHandles,
    /// 24B push（偏移见 [`pack_denoise_push`]）。
    pub push: [u8; GI_DENOISE_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(1/4 像素数 / 64)。
    pub groups: u32,
    /// 帧 UBO（set0；只保证集序一致）。
    pub frame_set: DescSet,
    /// G-buffer 采样集（b1 normal / b3 depth）。
    pub gbuffer_set: DescSet,
    /// 目标集（读 b0/b1、写 b2/b3）。
    pub target_set: DescSet,
}

impl GiDenoiseDraw {
    /// 录一次去噪 dispatch（set 序 = 帧 UBO / G-buffer / 目标）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.frame_set, self.gbuffer_set, self.target_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 全分辨率双边上采样的录制输入。
#[derive(Clone, Copy)]
pub struct GiUpsampleDraw {
    pub pass: ComputeHandles,
    /// 24B push（偏移见 [`pack_upsample_push`]）。
    pub push: [u8; GI_UPSAMPLE_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(全分辨率像素数 / 64)。
    pub groups: u32,
    /// 帧 UBO。
    pub frame_set: DescSet,
    /// G-buffer 采样集（全分辨率 normal / depth）。
    pub gbuffer_set: DescSet,
    /// 目标集（b0/b1 读 1/4、b4 写全分辨率）。
    pub target_set: DescSet,
}

impl GiUpsampleDraw {
    /// 录一次上采样 dispatch（set 序 = 帧 UBO / G-buffer / 目标）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[self.frame_set, self.gbuffer_set, self.target_set],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 查表 dispatch 的录制输入。
#[derive(Clone, Copy)]
pub struct GiScreenGatherDraw {
    pub pass: ComputeHandles,
    /// 32B push（偏移见 [`pack_screen_gather_push`]）。
    pub push: [u8; GI_SCREEN_GATHER_PUSH_SIZE as usize],
    /// dispatch 组数 = ceil(1/4 像素数 / 64)。
    pub groups: u32,
    /// 帧 UBO（inv_view_proj / camera_pos）。
    pub frame_set: DescSet,
    /// GI 集（cell→patch / 统计 / GI UBO / 时域辐照度）。
    pub gi_set: DescSet,
    /// G-buffer 采样集（b1 normal / b3 depth）。
    pub gbuffer_set: DescSet,
    /// 本段目标集（两张 1/4 图的 A/B 组）。
    pub target_set: DescSet,
}

impl GiScreenGatherDraw {
    /// 录一次查表 dispatch（set 序 = 帧 UBO / GI / G-buffer / 目标）。
    pub(crate) fn record(&self, device: &Device, cb: vk::CommandBuffer) {
        self.pass.record(
            device,
            cb,
            &[
                self.frame_set,
                self.gi_set,
                self.gbuffer_set,
                self.target_set,
            ],
            &self.push,
            self.groups,
            Handoff::ComputeReadWrite,
        );
    }
}

/// 查表 push：低/全分辨率尺寸 + 帧号 + 法线来源 + 样本数。
#[must_use]
pub fn pack_screen_gather_push(
    low: vk::Extent2D,
    full: vk::Extent2D,
    frame_index: u32,
    normal_source: u32,
    samples: u32,
) -> [u8; GI_SCREEN_GATHER_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_SCREEN_GATHER_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[
            low.width,
            low.height,
            full.width,
            full.height,
            frame_index,
            normal_source,
            samples,
            0,
        ],
    );
    push
}

/// 去噪 push：1/4 与全分辨率尺寸 + 采样步长。
#[must_use]
pub fn pack_denoise_push(
    low: vk::Extent2D,
    full: vk::Extent2D,
    step: u32,
) -> [u8; GI_DENOISE_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_DENOISE_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[low.width, low.height, full.width, full.height, step, 0],
    );
    push
}

/// 上采样 push：1/4 与全分辨率尺寸 + 模式（0 双边 / 1 纯双线性）。
#[must_use]
pub fn pack_upsample_push(
    low: vk::Extent2D,
    full: vk::Extent2D,
    mode: u32,
) -> [u8; GI_UPSAMPLE_PUSH_SIZE as usize] {
    let mut push = [0u8; GI_UPSAMPLE_PUSH_SIZE as usize];
    put_u32s(
        &mut push,
        0,
        &[low.width, low.height, full.width, full.height, mode, 0],
    );
    push
}

/// 全分辨率上采样的 dispatch 组数 = ceil(像素数 / 64)。
#[must_use]
pub fn upsample_groups(extent: vk::Extent2D) -> u32 {
    let pixels = u64::from(extent.width.max(1)) * u64::from(extent.height.max(1));
    pixels.div_ceil(64) as u32
}

/// 去噪的采样步长序列（第 k 遍用 `STEPS[k % 4]`，Unity 同序）。
pub const DENOISE_STEPS: [u32; 4] = [1, 2, 2, 3];

/// 一张 1/4 目标（image + 独占显存），Drop 时按相反顺序回收。
struct ScgiImage {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for ScgiImage {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 1/4 屏幕查表的两张中间目标（各双缓冲）+ 常驻描述符集。
#[derive(bevy::prelude::Resource)]
pub struct ScgiTargets {
    /// 设备句柄（Drop 里回收池与布局用）。
    device: Device,
    /// 描述符集布局：**只建一次**，resize 不重建（G-buffer 悬空教训）。
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 1/4 尺寸（0 = 还没建过）。
    extent: vk::Extent2D,
    /// 全分辨率尺寸（`scgi_irradiance` 的尺寸）。
    full_extent: vk::Extent2D,
    l0: Vec<ScgiImage>,
    l1: Vec<ScgiImage>,
    /// 全分辨率辐照度（单缓冲）。
    irradiance: Vec<ScgiImage>,
    l0_views: Vec<vk::ImageView>,
    l1_views: Vec<vk::ImageView>,
    irradiance_view: Vec<vk::ImageView>,
    /// 主方向集：b0/b1 = P0、b2/b3 = P1（查表写 P0、去噪 P0→P1 用它）。
    set_main: vk::DescriptorSet,
    /// 反方向集：b0/b1 = P1、b2/b3 = P0（去噪 P1→P0、上采样读 P1 用它）。
    set_reverse: vk::DescriptorSet,
    /// 采样集布局（合成 pass 用，只建一次）。
    sample_layout: vk::DescriptorSetLayout,
    sample_pool: vk::DescriptorPool,
    /// 合成 pass 读的那张：b0 = 全分辨率辐照度。
    sample_set: vk::DescriptorSet,
    /// 目标是否已写过（首帧写之前内容未定义，读侧靠 a 判"无数据"）。
    initialized: AtomicBool,
}

impl ScgiTargets {
    /// 建布局与描述符池（尺寸无关部分）。目标图在首次 [`Self::ensure`] 时建。
    ///
    /// # Errors
    /// 布局/池/描述符集创建失败。
    pub fn new(ctx: &Context) -> Result<Self> {
        let device = &ctx.device;
        let layout = SCGI_TARGET_SET_DESC.create(device)?;
        let pool_sizes = SCGI_TARGET_SET_DESC.pool_sizes_vec(2);
        let pool = unsafe {
            device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(2)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .inspect_err(|_| device.destroy_descriptor_set_layout(layout, None))?
        };
        let layouts = [layout, layout];
        let (set_main, set_reverse) = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => (sets[0], sets[1]),
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_pool(pool, None);
                    device.destroy_descriptor_set_layout(layout, None);
                }
                return Err(e.into());
            }
        };
        let sample_layout = SCGI_SAMPLE_SET_DESC.create(device)?;
        let sample_pool_sizes = SCGI_SAMPLE_SET_DESC.pool_sizes_vec(1);
        let sample_pool = unsafe {
            device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sample_pool_sizes),
                    None,
                )
                .inspect_err(|_| device.destroy_descriptor_set_layout(sample_layout, None))?
        };
        let sample_layouts = [sample_layout];
        let sample_set = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(sample_pool)
                    .set_layouts(&sample_layouts),
            )
        } {
            Ok(sets) => sets[0],
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_pool(sample_pool, None);
                    device.destroy_descriptor_set_layout(sample_layout, None);
                    device.destroy_descriptor_pool(pool, None);
                    device.destroy_descriptor_set_layout(layout, None);
                }
                return Err(e.into());
            }
        };
        Ok(Self {
            device: device.clone(),
            layout,
            pool,
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            full_extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            l0: Vec::new(),
            l1: Vec::new(),
            irradiance: Vec::new(),
            l0_views: Vec::new(),
            l1_views: Vec::new(),
            irradiance_view: Vec::new(),
            set_main,
            set_reverse,
            sample_layout,
            sample_pool,
            sample_set,
            initialized: AtomicBool::new(false),
        })
    }

    /// 尺寸变了就重建两张目标图并重写描述符集；同尺寸空操作。
    ///
    /// # Errors
    /// 图/视图/描述符写失败（失败时旧图已清空，下次 ensure 会再试）。
    pub fn ensure(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        let low = low_extent(extent);
        if low == self.extent && !self.l0.is_empty() {
            return Ok(());
        }
        let device = &ctx.device;
        let full = vk::Extent2D {
            width: extent.width.max(1),
            height: extent.height.max(1),
        };
        // 先清旧的（Drop 回收 image/memory/view）
        self.drop_targets();
        let mut l0 = Vec::with_capacity(SCGI_BUFFERS);
        let mut l1 = Vec::with_capacity(SCGI_BUFFERS);
        for _ in 0..SCGI_BUFFERS {
            l0.push(create_target(ctx, low, SCGI_L0_FORMAT)?);
            l1.push(create_target(ctx, low, SCGI_L1_FORMAT)?);
        }
        let irradiance = vec![create_target(ctx, full, SCGI_IRRADIANCE_FORMAT)?];
        let mut l0_views = Vec::with_capacity(SCGI_BUFFERS);
        let mut l1_views = Vec::with_capacity(SCGI_BUFFERS);
        for img in &l0 {
            l0_views.push(create_view(device, img.image, SCGI_L0_FORMAT)?);
        }
        for img in &l1 {
            l1_views.push(create_view(device, img.image, SCGI_L1_FORMAT)?);
        }
        let irradiance_view = vec![create_view(
            device,
            irradiance[0].image,
            SCGI_IRRADIANCE_FORMAT,
        )?];
        // 主方向：b0/b1 = P0、b2/b3 = P1；反方向交换两组
        for (set, forward) in [(self.set_main, true), (self.set_reverse, false)] {
            let (first_l0, first_l1, second_l0, second_l1) = if forward {
                (l0_views[0], l1_views[0], l0_views[1], l1_views[1])
            } else {
                (l0_views[1], l1_views[1], l0_views[0], l1_views[0])
            };
            let mut writer = DescriptorWriter::new(&SCGI_TARGET_SET_DESC, set);
            writer
                .image(0, first_l0, vk::ImageLayout::GENERAL)
                .image(1, first_l1, vk::ImageLayout::GENERAL)
                .image(2, second_l0, vk::ImageLayout::GENERAL)
                .image(3, second_l1, vk::ImageLayout::GENERAL)
                .image(4, irradiance_view[0], vk::ImageLayout::GENERAL);
            writer.submit(device)?;
        }
        self.extent = low;
        self.full_extent = full;
        self.l0 = l0;
        self.l1 = l1;
        self.irradiance = irradiance;
        self.l0_views = l0_views;
        self.l1_views = l1_views;
        let mut sample_writer = DescriptorWriter::new(&SCGI_SAMPLE_SET_DESC, self.sample_set);
        sample_writer.image(0, irradiance_view[0], vk::ImageLayout::GENERAL);
        sample_writer.submit(device)?;
        self.irradiance_view = irradiance_view;
        // 全部目标图一次性 UNDEFINED → GENERAL（之后恒 GENERAL）：迁移放在这里而不是每帧，
        // 因为合成 pass 也会绑这张图，而 GI 关掉的帧不录 ④.5 段——那类帧同样需要合法布局。
        self.transition_all(ctx)?;
        self.initialized.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// 登记给管线组装（与 [`GiSetLayout::set_layout`] 同形）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&SCGI_TARGET_SET_DESC, self.layout)
    }

    /// 主方向目标集（b0/b1 = P0、b2/b3 = P1）。
    #[must_use]
    pub fn set(&self) -> DescSet {
        DescSet::new(&SCGI_TARGET_SET_DESC, self.set_main)
    }

    /// 反方向目标集（b0/b1 = P1、b2/b3 = P0）。
    #[must_use]
    pub fn set_reverse(&self) -> DescSet {
        DescSet::new(&SCGI_TARGET_SET_DESC, self.set_reverse)
    }

    /// 全分辨率尺寸（0×0 = 还没有目标）。
    pub fn full_extent(&self) -> vk::Extent2D {
        self.full_extent
    }

    /// 1/4 尺寸（0×0 = 还没有目标）。
    pub fn low_extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// 本段目标的显存总量（字节）：1/4 两张各双缓冲 + 全分辨率一张。
    #[must_use]
    pub fn memory_bytes(&self) -> u64 {
        let low_pixels = u64::from(self.extent.width) * u64::from(self.extent.height);
        let full_pixels = u64::from(self.full_extent.width) * u64::from(self.full_extent.height);
        let per_pixel = SCGI_L0_BYTES + SCGI_L1_BYTES;
        per_pixel * low_pixels * SCGI_BUFFERS as u64 + SCGI_IRRADIANCE_BYTES * full_pixels
    }

    /// 是否已经写过至少一帧（调试视图用）。
    #[must_use]
    pub fn initialized(&self) -> bool {
        self.initialized.load(Ordering::Relaxed)
    }

    /// 标记"本帧已写"（`prepare_gi` 录完查表后调）。
    pub fn mark_written(&self) {
        self.initialized.store(true, Ordering::Relaxed);
    }

    /// 采样集布局（登记进 `SetLayouts` 给合成管线对账）。
    #[must_use]
    pub fn sample_layout(&self) -> SetLayout {
        SetLayout::new(&SCGI_SAMPLE_SET_DESC, self.sample_layout)
    }

    /// 合成 pass 的采样集（b0 = 全分辨率辐照度）。
    #[must_use]
    pub fn sample_set(&self) -> DescSet {
        DescSet::new(&SCGI_SAMPLE_SET_DESC, self.sample_set)
    }

    /// 把新建/重建出来的目标图一次性从 UNDEFINED 迁到 GENERAL（一次性提交 + 等空闲）。
    ///
    /// 放在 ensure 里而不是每帧录屏障：合成 pass 每帧都绑这张图，而 GI 关掉的帧不录 ④.5 段，
    /// 那类帧也得有合法布局。resize 本来就让帧循环暂停重建，这里等空闲不进入每帧路径。
    fn transition_all(&self, ctx: &Context) -> Result<()> {
        let mut images: Vec<vk::Image> = self.l0.iter().map(|i| i.image).collect();
        images.extend(self.l1.iter().map(|i| i.image));
        images.extend(self.irradiance.iter().map(|i| i.image));
        if images.is_empty() {
            return Ok(());
        }
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barriers: Vec<vk::ImageMemoryBarrier> = images
            .iter()
            .map(|image| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(*image)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            })
            .collect();
        let device = &ctx.device;
        unsafe {
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(ctx.queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )?;
            let cbs = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?;
            let cb = cbs[0];
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            device.end_command_buffer(cb)?;
            let cbs_to_submit = [cb];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
            device.queue_submit(ctx.queue, &[submit], vk::Fence::null())?;
            device.queue_wait_idle(ctx.queue)?;
            device.destroy_command_pool(pool, None);
        }
        Ok(())
    }

    fn drop_targets(&mut self) {
        for view in self
            .l0_views
            .drain(..)
            .chain(self.l1_views.drain(..))
            .chain(self.irradiance_view.drain(..))
        {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        self.l0.clear();
        self.l1.clear();
        self.irradiance.clear();
    }
}

impl Drop for ScgiTargets {
    fn drop(&mut self) {
        self.drop_targets();
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.layout, None);
            self.device.destroy_descriptor_pool(self.sample_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.sample_layout, None);
        }
    }
}

/// 1/4 尺寸 = 全分辨率向上取整除以 4（下限 1）。
pub fn low_extent(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: extent.width.max(1).div_ceil(4),
        height: extent.height.max(1).div_ceil(4),
    }
}

/// 建一张 `GENERAL` 布局、可 storage 写也可 sampled 读的 2D 图。
fn create_target(ctx: &Context, extent: vk::Extent2D, format: vk::Format) -> Result<ScgiImage> {
    let device = &ctx.device;
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: extent.width.max(1),
                    height: extent.height.max(1),
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(
                    vk::ImageUsageFlags::STORAGE
                        | vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                // vkCreateImage 只接受 UNDEFINED/PREINITIALIZED：之后由一道显式屏障
                // 送到 GENERAL，并一直留在那里（内容每帧整体重写）
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }?;
    let memory = match create_bound_memory(ctx, image) {
        Ok(memory) => memory,
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            return Err(e);
        }
    };
    Ok(ScgiImage {
        device: device.clone(),
        image,
        memory,
    })
}

fn create_view(device: &Device, image: vk::Image, format: vk::Format) -> Result<vk::ImageView> {
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(range);
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

/// image 的 dedicated memory（DEVICE_LOCAL、恰量分配、offset 0）。
fn create_bound_memory(ctx: &Context, image: vk::Image) -> Result<vk::DeviceMemory> {
    unsafe {
        let reqs = ctx.device.get_image_memory_requirements(image);
        let memory_type = ctx.memory_contract().find_type(
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        )?;
        let memory = ctx.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(memory_type),
            None,
        )?;
        if let Err(e) = ctx.device.bind_image_memory(image, memory, 0) {
            ctx.device.free_memory(memory, None);
            return Err(VulkanError::Vk(e));
        }
        Ok(memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_extent_rounds_up() {
        assert_eq!(
            low_extent(vk::Extent2D {
                width: 1920,
                height: 1080
            }),
            vk::Extent2D {
                width: 480,
                height: 270
            }
        );
        assert_eq!(
            low_extent(vk::Extent2D {
                width: 1921,
                height: 1081
            }),
            vk::Extent2D {
                width: 481,
                height: 271
            }
        );
        assert_eq!(
            low_extent(vk::Extent2D {
                width: 0,
                height: 0
            }),
            vk::Extent2D {
                width: 1,
                height: 1
            }
        );
    }

    #[test]
    fn gather_push_layout() {
        let push = pack_screen_gather_push(
            vk::Extent2D {
                width: 480,
                height: 270,
            },
            vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            42,
            1,
            3,
        );
        assert_eq!(push.len(), 32);
        let words: Vec<u32> = push
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, vec![480, 270, 1920, 1080, 42, 1, 3, 0]);
    }
}
