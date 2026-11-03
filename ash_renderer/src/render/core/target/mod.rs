//! 附件图像：颜色目标、深度目标（图像 + 视图 + 内存）与主链用的附件格式常量。
//! 两者共用 `attachment` 的创建链。

mod attachment;
pub(in crate::render) mod color;
pub(in crate::render) mod depth;
