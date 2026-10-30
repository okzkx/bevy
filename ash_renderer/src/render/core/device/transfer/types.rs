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

/// 将紧密排列的 2D 图像 mip0 从 staging 拷入；多级图像的其余层由 blit 生成。
#[derive(Clone, Copy, Debug)]
pub struct StagingImageCopy {
    /// 目标图像须带 `TRANSFER_DST` 和 `TRANSFER_SRC` 用途。
    pub image: vk::Image,
    /// staging buffer 中的源偏移（字节），须按 texel block 对齐。
    pub src_offset: u64,
    pub width: u32,
    pub height: u32,
    /// 已分配的 mip 层数；为 1 时不生成 mip 链。
    pub mip_levels: u32,
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
    /// 图像所有权转移及最终的 shader 可采样布局转换。
    pub image_releases: Vec<ImageRelease>,
}
