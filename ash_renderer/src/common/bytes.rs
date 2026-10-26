//! 定长 GPU 记录的字节打包标准件：标量一律小端（`to_le_bytes`），矩阵按列主序
//! （`Mat4::to_cols_array`）展平，与 WGSL storage/uniform/push 的读法一致。
//!
//! 偏移由调用方按各自布局镜像（`offset_of!` 静态断言表）给出；`out` 是定长记录
//! 缓冲，偏移越界属布局写错，按切片下标语义当场失败。

use bevy::math::Mat4;

/// `out[off..off + 4]` ← `v`（小端）。
pub fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// `out[off..off + 4]` ← `v`（小端）。
pub fn put_f32(out: &mut [u8], off: usize, v: f32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// 自 `off` 起紧排一串 u32（步长 4B）。
pub fn put_u32s(out: &mut [u8], off: usize, vs: &[u32]) {
    for (i, &v) in vs.iter().enumerate() {
        put_u32(out, off + i * 4, v);
    }
}

/// 自 `off` 起紧排一串 f32（步长 4B；vec3 只写 12B，w 垫由调用方保持 0）。
pub fn put_f32s(out: &mut [u8], off: usize, vs: &[f32]) {
    for (i, &v) in vs.iter().enumerate() {
        put_f32(out, off + i * 4, v);
    }
}

/// `out[off..off + 16]` ← vec4。
pub fn put_vec4(out: &mut [u8], off: usize, v: &[f32; 4]) {
    put_f32s(out, off, v);
}

/// `out[off..off + 64]` ← mat4x4f（列主序：flat = col * 4 + row）。
pub fn put_mat4(out: &mut [u8], off: usize, m: &Mat4) {
    put_f32s(out, off, &m.to_cols_array());
}

#[cfg(test)]
mod tests {
    use super::{put_mat4, put_u32, put_vec4};
    use bevy::math::{Mat4, Vec3};

    /// 列主序展平与小端序：逐 4B 还原等于 `to_cols_array`。
    #[test]
    fn mat4_cols_little_endian() {
        let m = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let mut out = [0u8; 68];
        put_mat4(&mut out, 4, &m);
        let cols = m.to_cols_array();
        for (i, c) in cols.iter().enumerate() {
            assert_eq!(out[4 + i * 4..8 + i * 4], c.to_le_bytes());
        }
        assert_eq!(out[..4], [0u8; 4], "偏移前的字节不被触碰");
    }

    #[test]
    fn scalar_and_vec4_offsets() {
        let mut out = [0u8; 24];
        put_u32(&mut out, 0, 0x0403_0201);
        put_vec4(&mut out, 8, &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(out[..4], [1, 2, 3, 4]);
        assert_eq!(out[4..8], [0u8; 4]);
        assert_eq!(out[20..24], 4.0f32.to_le_bytes());
    }
}
