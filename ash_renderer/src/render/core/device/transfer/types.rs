//! 设备传输批次与完成票据类型。

use ash::vk;

/// 单调递增的完成票据；0 表示空批次。
pub type Ticket = u64;

/// buffer 到 buffer 的拷贝；偏移和长度须为 4 的倍数。
#[derive(Clone, Copy, Debug)]
pub struct CopyRegion {
    pub src: vk::Buffer,
    pub dst: vk::Buffer,
    pub src_offset: u64,
    pub dst_offset: u64,
    pub size: u64,
}

/// 从本批 staging buffer 拷贝到目标 buffer。
#[derive(Clone, Copy, Debug)]
pub struct StagingCopy {
    /// 目标 buffer，如顶点池或索引池。
    pub dst: vk::Buffer,
    /// 本批 staging buffer 中的源偏移（字节）。
    pub src_offset: u64,
    pub dst_offset: u64,
    pub size: u64,
}

/// 将 buffer 所有权转移到目标队列族。
#[derive(Clone, Copy, Debug)]
pub struct Release {
    pub buffer: vk::Buffer,
    /// 接收所有权的队列族。
    pub to_family: u32,
}

/// 将紧密排列的 2D 图像 mip0 从 staging 拷入；多级未压缩图像的其余层由 blit 生成，
/// 块压缩图像的各级由本段按块算术逐级拷入（mip 全链离线/烘焙预生成）。
#[derive(Clone, Copy, Debug)]
pub struct StagingImageCopy {
    /// 目标图像须带 `TRANSFER_DST` 和 `TRANSFER_SRC` 用途。
    pub image: vk::Image,
    /// staging buffer 中的源偏移（字节），须按 texel block 对齐（块压缩再乘块字节倍数）。
    pub src_offset: u64,
    pub width: u32,
    pub height: u32,
    /// 已分配的 mip 层数；为 1 时不生成 mip 链。
    pub mip_levels: u32,
    /// 图像格式：块压缩格式走逐级块拷贝，未压缩格式只拷 mip0。
    pub format: vk::Format,
}

/// 块压缩格式的块字节数：BC1/BC4 每块 8B，其余家族成员 16B；非压缩格式返回 0。
#[must_use]
pub fn block_bytes(format: vk::Format) -> u32 {
    match format {
        vk::Format::BC1_RGB_UNORM_BLOCK
        | vk::Format::BC1_RGB_SRGB_BLOCK
        | vk::Format::BC1_RGBA_UNORM_BLOCK
        | vk::Format::BC1_RGBA_SRGB_BLOCK
        | vk::Format::BC4_UNORM_BLOCK => 8,
        vk::Format::BC2_UNORM_BLOCK
        | vk::Format::BC2_SRGB_BLOCK
        | vk::Format::BC3_UNORM_BLOCK
        | vk::Format::BC3_SRGB_BLOCK
        | vk::Format::BC5_UNORM_BLOCK
        | vk::Format::BC6H_SFLOAT_BLOCK
        | vk::Format::BC6H_UFLOAT_BLOCK
        | vk::Format::BC7_UNORM_BLOCK
        | vk::Format::BC7_SRGB_BLOCK => 16,
        _ => 0,
    }
}

/// 一级 mip 的压缩字节数：`ceil(w/4) × ceil(h/4)` 块 × 块字节（宽高按像素算，
/// 末级小于 4 的也是整块）。非压缩格式返回 0（无意义，勿用）。
#[must_use]
pub fn level_bytes(format: vk::Format, width: u32, height: u32) -> u64 {
    u64::from(block_bytes(format)) * u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4))
}

/// 是否块压缩格式（Vulkan 纹理压缩三族的块算术与 blit 限制都由它分叉）。
#[must_use]
pub fn is_block_compressed(format: vk::Format) -> bool {
    block_bytes(format) > 0
}

/// 将图像所有权转移到目标队列族并切换到可采样布局。
#[derive(Clone, Copy, Debug)]
pub struct ImageRelease {
    pub image: vk::Image,
    /// 接收所有权的队列族。
    pub to_family: u32,
}

/// 同一次 transfer 队列提交的全部工作。
#[derive(Default)]
pub struct UploadBatch {
    /// 从偏移 0 开始连续排列的 staging 字节。
    pub staging: Vec<u8>,
    /// staging 到 buffer 的拷贝段。
    pub uploads: Vec<StagingCopy>,
    /// staging 到图像的拷贝段；多级图像由 graphics 队列续批完成。
    pub image_uploads: Vec<StagingImageCopy>,
    /// 设备间拷贝，如池扩容迁移。
    pub device_copies: Vec<CopyRegion>,
    /// buffer 所有权转移；源与目标队列族相同时留空。
    pub releases: Vec<Release>,
    /// 图像所有权转移及最终的 shader 可采样布局转换；只对同批 `image_uploads` 里的图像生效。
    pub image_releases: Vec<ImageRelease>,
}
