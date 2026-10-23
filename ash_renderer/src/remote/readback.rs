//! GPU 读回字节的小端解析标准件（BRP 读数族共用）。读回段长度由 handler
//! 按容量/dispatch 上界自定，按偏移取值越界 = 槽号落在读回段外（读数侧
//! 不变式破缺），回 `BrpError::internal`——协议报错，不炸宿主。

use super::BrpError;

/// 整段按 4 字节小端字解析；尾部不足 4 字节的余量丢弃（读回段按字对齐分配，
/// 余量只会来自调用方传错长度）。
pub(crate) fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

/// `offset` 起 `len` 字节的子段；越界报 internal。
pub(crate) fn bytes_at(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], BrpError> {
    offset
        .checked_add(len)
        .and_then(|end| bytes.get(offset..end))
        .ok_or_else(|| out_of_range(bytes, offset, len))
}

/// `offset` 处的小端 u32；越界报 internal。
pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, BrpError> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|c| u32::from_le_bytes(*c))
        .ok_or_else(|| out_of_range(bytes, offset, 4))
}

/// `offset` 处的小端 f32；越界报 internal。
pub(crate) fn f32_at(bytes: &[u8], offset: usize) -> Result<f32, BrpError> {
    u32_at(bytes, offset).map(f32::from_bits)
}

fn out_of_range(bytes: &[u8], offset: usize, len: usize) -> BrpError {
    BrpError::internal(format!(
        "读回越界：偏移 {offset} 取 {len} 字节，读回段仅 {} 字节",
        bytes.len()
    ))
}
