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
pub const SCGI_L0_BYTES: u64 = 8;
/// `scgi_l1` 每像素字节数（RGBA8）。
pub const SCGI_L1_BYTES: u64 = 4;
/// 目标图总数（两格式 × 双缓冲 + 全分辨率一张）：`ensure` 里一次性迁到 GENERAL 的那批。
pub const SCGI_IMAGES: usize = SCGI_BUFFERS * 2 + 1;

const _: () = assert!(SCGI_IMAGES == 5);

/// 全分辨率辐照度目标格式：rgb = 按全分辨率法线求值的 E(n)，a = 覆盖率。
pub const SCGI_IRRADIANCE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// `scgi_irradiance` 每像素字节数。
pub const SCGI_IRRADIANCE_BYTES: u64 = 8;

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
    /// 布局初始化的一次性命令池；提交后等待失败时保留到排空，不能销毁 pending 命令。
    transition_pool: vk::CommandPool,
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
        // 取得的句柄立即归 Self：任意创建步骤失败，都由同一个 Drop 回收已建部分。
        let mut this = Self {
            device: device.clone(),
            layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            extent: vk::Extent2D::default(),
            full_extent: vk::Extent2D::default(),
            l0: Vec::new(),
            l1: Vec::new(),
            irradiance: Vec::new(),
            l0_views: Vec::new(),
            l1_views: Vec::new(),
            irradiance_view: Vec::new(),
            set_main: vk::DescriptorSet::null(),
            set_reverse: vk::DescriptorSet::null(),
            sample_layout: vk::DescriptorSetLayout::null(),
            sample_pool: vk::DescriptorPool::null(),
            sample_set: vk::DescriptorSet::null(),
            transition_pool: vk::CommandPool::null(),
            initialized: AtomicBool::new(false),
        };
        this.layout = SCGI_TARGET_SET_DESC.create(device)?;
        let pool_sizes = SCGI_TARGET_SET_DESC.pool_sizes_vec(2);
        this.pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(2)
                    .pool_sizes(&pool_sizes),
                None,
            )?
        };
        let layouts = [this.layout, this.layout];
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.pool)
                    .set_layouts(&layouts),
            )?
        };
        this.set_main = sets[0];
        this.set_reverse = sets[1];
        this.sample_layout = SCGI_SAMPLE_SET_DESC.create(device)?;
        let sample_pool_sizes = SCGI_SAMPLE_SET_DESC.pool_sizes_vec(1);
        this.sample_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sample_pool_sizes),
                None,
            )?
        };
        let sample_layouts = [this.sample_layout];
        let sample_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(this.sample_pool)
                    .set_layouts(&sample_layouts),
            )?
        };
        this.sample_set = sample_sets[0];
        Ok(this)
    }

    /// 尺寸变了就重建两张目标图并重写描述符集；同尺寸空操作。
    ///
    /// # Errors
    /// 图/视图/描述符写或布局迁移失败。已建部分仍归 Self，尺寸保持未完成状态；
    /// 下次 ensure 清理后重试，或由退出排空之后的 Drop 回收。
    pub fn ensure(&mut self, ctx: &Context, extent: vk::Extent2D) -> Result<()> {
        if target_extents_match(self.extent, self.full_extent, extent) && !self.l0.is_empty() {
            return Ok(());
        }
        let device = &ctx.device;
        let low = low_extent(extent);
        let full = nonzero_extent(extent);
        // 初始化提交后等待失败，保留池与附件；重试须先排空才能回收这批资源。
        if self.transition_pool != vk::CommandPool::null() {
            unsafe { device.queue_wait_idle(ctx.queue)? };
            self.drop_transition_pool();
        }
        self.drop_targets();
        // Image/view 立即归 Self，后续创建或描述符写入失败也不会遗失裸句柄。
        for _ in 0..SCGI_BUFFERS {
            self.l0.push(create_target(ctx, low, SCGI_L0_FORMAT)?);
            self.l1.push(create_target(ctx, low, SCGI_L1_FORMAT)?);
        }
        self.irradiance
            .push(create_target(ctx, full, SCGI_IRRADIANCE_FORMAT)?);
        for img in &self.l0 {
            self.l0_views
                .push(create_view(device, img.image, SCGI_L0_FORMAT)?);
        }
        for img in &self.l1 {
            self.l1_views
                .push(create_view(device, img.image, SCGI_L1_FORMAT)?);
        }
        self.irradiance_view.push(create_view(
            device,
            self.irradiance[0].image,
            SCGI_IRRADIANCE_FORMAT,
        )?);
        // 主方向：b0/b1 = P0、b2/b3 = P1；反方向交换两组
        for (set, forward) in [(self.set_main, true), (self.set_reverse, false)] {
            let (first_l0, first_l1, second_l0, second_l1) = if forward {
                (
                    self.l0_views[0],
                    self.l1_views[0],
                    self.l0_views[1],
                    self.l1_views[1],
                )
            } else {
                (
                    self.l0_views[1],
                    self.l1_views[1],
                    self.l0_views[0],
                    self.l1_views[0],
                )
            };
            let mut writer = DescriptorWriter::new(&SCGI_TARGET_SET_DESC, set);
            writer
                .image(0, first_l0, vk::ImageLayout::GENERAL)
                .image(1, first_l1, vk::ImageLayout::GENERAL)
                .image(2, second_l0, vk::ImageLayout::GENERAL)
                .image(3, second_l1, vk::ImageLayout::GENERAL)
                .image(4, self.irradiance_view[0], vk::ImageLayout::GENERAL);
            writer.submit(device)?;
        }
        let mut sample_writer = DescriptorWriter::new(&SCGI_SAMPLE_SET_DESC, self.sample_set);
        sample_writer.image(0, self.irradiance_view[0], vk::ImageLayout::GENERAL);
        sample_writer.submit(device)?;
        // 全部目标图一次性 UNDEFINED → GENERAL（之后恒 GENERAL）：迁移放在这里而不是每帧，
        // 因为合成 pass 也会绑这张图，而 GI 关掉的帧不录 ④.5 段——那类帧同样需要合法布局。
        self.transition_all(ctx)?;
        // 尺寸是完成标记：只有全部建成且布局迁移成功，才允许同尺寸 fast-path 复用。
        self.extent = low;
        self.full_extent = full;
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

    /// 全分辨率辐照度图（BRP 读回用）。
    #[must_use]
    pub fn irradiance_image(&self) -> vk::Image {
        self.irradiance[0].image
    }

    /// 1/4 分辨率 L0 目标（0 = P0、1 = P1；BRP 逐块诊断读回用）。
    ///
    /// # Panics
    /// `index` 超出双缓冲数量。
    #[must_use]
    pub fn low_l0_image(&self, index: usize) -> vk::Image {
        self.l0[index].image
    }

    /// 1/4 分辨率 L1 目标（0 = P0、1 = P1；BRP 逐块诊断读回用）。
    ///
    /// # Panics
    /// `index` 超出双缓冲数量。
    #[must_use]
    pub fn low_l1_image(&self, index: usize) -> vk::Image {
        self.l1[index].image
    }

    /// 读回 `scgi_irradiance`（RGBA16F；读回后把布局迁回 GENERAL，不影响后续采样）。
    ///
    /// # Errors
    /// 一次性回读资源创建/提交/等待失败。
    pub fn readback_irradiance(&self, ctx: &Context) -> Result<Vec<u8>> {
        use crate::render::core::device::readback::OneShotReadback;
        use crate::render::core::record::barrier::{ImageBarrier, MemoryBarrierDesc};

        let extent = self.full_extent;
        let byte_size = u64::from(extent.width) * u64::from(extent.height) * SCGI_IRRADIANCE_BYTES;
        let mut out = vec![0u8; byte_size as usize];
        let device = &ctx.device;
        let image = self.irradiance_image();
        unsafe {
            let readback = OneShotReadback::begin(ctx, byte_size)?;
            MemoryBarrierDesc {
                src_stage: vk::PipelineStageFlags::ALL_COMMANDS,
                src_access: vk::AccessFlags::MEMORY_WRITE,
                dst_stage: vk::PipelineStageFlags::TRANSFER,
                dst_access: vk::AccessFlags::TRANSFER_READ,
            }
            .record(device, readback.cb);
            ImageBarrier::transition(
                image,
                vk::ImageAspectFlags::COLOR,
                (
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::TRANSFER,
                ),
                (
                    vk::AccessFlags::SHADER_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                ),
            )
            .record(device, readback.cb);
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                readback.cb,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.staging,
                std::slice::from_ref(&region),
            );
            // 读回后迁回 GENERAL：合成 pass 每帧都要按 GENERAL 采样这张图
            ImageBarrier::transition(
                image,
                vk::ImageAspectFlags::COLOR,
                (
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                ),
                (
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                ),
                (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::SHADER_READ),
            )
            .record(device, readback.cb);
            readback.finish_and_read(ctx, &mut out)?;
        }
        Ok(out)
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
    fn transition_all(&mut self, ctx: &Context) -> Result<()> {
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
        self.transition_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(ctx.queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )?
        };
        let pool = self.transition_pool;
        let mut submitted = false;
        let result = unsafe {
            (|| -> Result<()> {
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
                submitted = true;
                device.queue_wait_idle(ctx.queue)?;
                Ok(())
            })()
        };
        // 未提交的失败可以立即回收；提交后的等待失败不能假定 GPU 已不再引用池与附件。
        if transition_pool_can_drop(submitted, result.is_ok()) {
            self.drop_transition_pool();
        }
        result
    }

    fn drop_transition_pool(&mut self) {
        if self.transition_pool != vk::CommandPool::null() {
            unsafe { self.device.destroy_command_pool(self.transition_pool, None) };
            self.transition_pool = vk::CommandPool::null();
        }
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
        self.extent = vk::Extent2D::default();
        self.full_extent = vk::Extent2D::default();
        self.initialized.store(false, Ordering::Relaxed);
    }
}

impl Drop for ScgiTargets {
    fn drop(&mut self) {
        // 退出排空由 teardown 先行；构造失败尚未提交，不存在在飞引用。
        self.drop_transition_pool();
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

fn transition_pool_can_drop(submitted: bool, completed: bool) -> bool {
    !submitted || completed
}

fn nonzero_extent(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: extent.width.max(1),
        height: extent.height.max(1),
    }
}

/// 1/4 尺寸相同不代表全分辨率相同：同一个 ceil/4 桶内的 resize 也要换辐照度附件。
fn target_extents_match(
    current_low: vk::Extent2D,
    current_full: vk::Extent2D,
    requested: vk::Extent2D,
) -> bool {
    current_low == low_extent(requested) && current_full == nonzero_extent(requested)
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

    const FILTER_SHADERS: [&str; 2] = [
        include_str!("../../../../shaders/scene/gi/denoise.wgsl"),
        include_str!("../../../../shaders/scene/gi/upsample.wgsl"),
    ];

    #[test]
    fn transition_pool_cleanup_preserves_pending_submission() {
        assert!(transition_pool_can_drop(false, false));
        assert!(transition_pool_can_drop(false, true));
        assert!(!transition_pool_can_drop(true, false));
        assert!(transition_pool_can_drop(true, true));
    }

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
    fn resize_within_same_low_extent_changes_full_target() {
        let original = vk::Extent2D {
            width: 1921,
            height: 1081,
        };
        let resized = vk::Extent2D {
            width: 1922,
            height: 1082,
        };
        let low = low_extent(original);
        assert_eq!(low, low_extent(resized));
        assert!(target_extents_match(low, original, original));
        assert!(!target_extents_match(low, original, resized));
        assert!(!target_extents_match(low, resized, original));
    }

    #[test]
    fn target_extents_normalize_zero_dimensions() {
        let requested = vk::Extent2D {
            width: 0,
            height: 0,
        };
        let full = vk::Extent2D {
            width: 1,
            height: 1,
        };
        assert!(target_extents_match(full, full, requested));
        assert!(!target_extents_match(requested, requested, requested));
    }

    #[test]
    fn image_byte_sizes_match_formats() {
        assert_eq!(SCGI_L0_BYTES, 4 * 2);
        assert_eq!(SCGI_L1_BYTES, 4);
        assert_eq!(SCGI_IRRADIANCE_BYTES, 4 * 2);
    }

    /// CPU/source 合同：读取 shader 常量，不代表 GPU fp16/UNORM 执行精度验收。
    fn shader_l1_scale() -> f32 {
        let shader = include_str!("../../../../shaders/common/gi_compression.wgsl");
        assert!(shader.contains("ratio * L1_ENCODE_SCALE + 0.5"));
        assert!(shader.contains("(encoded - 0.5) / L1_ENCODE_SCALE"));
        let scale = shader.lines().find_map(|line| {
            line.trim()
                .strip_prefix("const L1_ENCODE_SCALE: f32 = ")?
                .strip_suffix(';')?
                .parse::<f32>()
                .ok()
        });
        assert_eq!(scale, Some(0.25));
        scale.unwrap_or_default()
    }

    #[test]
    fn l1_axis_ratio_two_survives_shader_encoding() {
        let scale = shader_l1_scale();
        for ratio in [-2.0f32, 0.0, 2.0] {
            let encoded = (ratio * scale + 0.5).clamp(0.0, 1.0);
            let decoded = (encoded - 0.5) / scale;
            assert!((decoded - ratio).abs() < 1e-6);
        }
    }

    #[test]
    fn filtering_decoded_coefficients_preserves_energy() {
        for shader in FILTER_SHADERS {
            assert!(shader.contains("l1_decode_axis(sample_l1.x, sample_l0.xyz) * weight"));
            assert!(shader.contains("l1_decode_axis(sample_l1.y, sample_l0.xyz) * weight"));
            assert!(shader.contains("l1_decode_axis(sample_l1.z, sample_l0.xyz) * weight"));
            assert!(shader.contains("acc_l0 / valid_weight_sum"));
        }
        assert!(FILTER_SHADERS[0].contains("l1_encode_axis(acc_l1x / valid_weight_sum, out_l0)"));
        assert!(FILTER_SHADERS[1].contains("scgi_evaluate(l0, l1x, l1y, l1z, normal)"));
        let scale = shader_l1_scale();
        let l0 = [100.0f32, 1.0];
        let ratio = [1.0f32, -1.0];
        let encoded = ratio.map(|r| (r * scale + 0.5).clamp(0.0, 1.0));
        let decoded_l1 = [
            l0[0] * (encoded[0] - 0.5) / scale,
            l0[1] * (encoded[1] - 0.5) / scale,
        ];
        let filtered_l0 = (l0[0] + l0[1]) * 0.5;
        let filtered_l1 = (decoded_l1[0] + decoded_l1[1]) * 0.5;
        assert!((filtered_l0 + filtered_l1 - 100.0).abs() < 1e-6);
        let averaged_ratio = ((encoded[0] + encoded[1]) * 0.5 - 0.5) / scale;
        assert!((filtered_l0 * (1.0 + averaged_ratio) - 50.5).abs() < 1e-6);
    }

    #[test]
    fn invalid_samples_reduce_coverage_not_valid_irradiance() {
        let attempted_terms = [
            "attempted_weight = attempted_weight + geometry_weight;",
            "attempted_weight = attempted_weight + weight;",
        ];
        for (shader, attempted_term) in FILTER_SHADERS.into_iter().zip(attempted_terms) {
            assert!(!shader.contains("sample_l0.w <= 0.0"));
            assert!(shader.contains(attempted_term));
            let attempted_offset = shader.find(attempted_term).unwrap_or(shader.len());
            let alpha_offset = shader
                .find("clamp(sample_l0.w, 0.0, 1.0)")
                .unwrap_or_default();
            assert!(attempted_offset < alpha_offset);
            assert!(shader.contains("* clamp(sample_l0.w, 0.0, 1.0)"));
            assert!(shader.contains("valid_weight_sum = valid_weight_sum + weight"));
            assert!(shader.contains("valid_weight_sum / attempted_weight"));
        }
        let weights = [1.0f32, 1.0];
        let alpha = [1.0f32, 0.0];
        let irradiance = [100.0f32, 0.0];
        let total_weight = weights[0] + weights[1];
        let covered_weight = weights[0] * alpha[0] + weights[1] * alpha[1];
        let filtered = (irradiance[0] * weights[0] * alpha[0]
            + irradiance[1] * weights[1] * alpha[1])
            / covered_weight;
        let coverage = covered_weight / total_weight;
        assert!((filtered - 100.0).abs() < 1e-6);
        assert!((coverage - 0.5).abs() < 1e-6);
        let ibl = 20.0;
        let injected = ibl * (1.0 - coverage) + filtered * coverage;
        assert!((injected - 60.0).abs() < 1e-6);
        // 另一张表面被几何权重拒绝时，不属于当前表面的缓存缺口。
        let geometry_weights = [1.0f32, 0.0];
        let attempted = geometry_weights[0] + geometry_weights[1];
        let covered = geometry_weights[0] * alpha[0] + geometry_weights[1] * alpha[1];
        assert!((covered / attempted - 1.0).abs() < 1e-6);
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
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        assert_eq!(words, vec![480, 270, 1920, 1080, 42, 1, 3, 0]);
    }
}
