//! 设备队列传输与上传批次类型。

mod types;
mod uploader;

pub(crate) use types::Ticket;
pub use types::{CopyRegion, ImageRelease, Release, StagingCopy, StagingImageCopy, UploadBatch};
pub use uploader::Uploader;
