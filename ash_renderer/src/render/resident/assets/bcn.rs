//! BCn 显存压缩的烘焙底座：贴图数据角色 → BCn 格式选型、RGBA8 的 CPU mip 链生成、
//! 逐级块编码。纯 CPU 无 Vulkan 对象，产物 [`BakedBcn`] 交缓存层与上传直通。
//!
//! 编码用 texpresso（libsquish 血统，RangeFit 快摄合档，rayon 并行）。通道约定源码
//! 核实：BC4 编 R 单通道（= glTF occlusion 读 R），BC5 编 R/G 双通道（= glTF 法线
//! XY）。metallicRoughness 要保住 G/B 通道语义需要 BC7，当前编码库没有，该角色
//! 不压缩，留在 RGBA8 现行路径。
//!
//! 坑：Vulkan 压缩格式要求 level-0 宽高是 4 的倍数，源图非 4 倍数时先边缘扩展再进
//! mip 链；各级压缩字节天然是块字节数的倍数，连排后 offset 无需额外对齐。
//! 块字节数与逐级字节数的正本在 `transfer::types`（上传录制端同一套算术）。

use ash::vk;
use bevy::log::info;
use texpresso;
use thiserror::Error;

use super::image_spec::full_mip_chain;
use crate::render::core::device::transfer::level_bytes;

/// 缓存键与缓存文件格式都挂这个版本号：编码行为变了，旧缓存自然失效。
pub const BAKE_VERSION: u32 = 1;

/// 贴图数据角色。枚举顺序即优先级：同一张图被多个角色引用时取最小者（数据角色
/// 优先于颜色角色——数据图用颜色格式会糊，颜色图用数据格式会丢通道）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TextureRole {
    /// glTF normalTexture：切线空间法线 XY（Z 采样后重建）。
    Normal,
    /// glTF metallicRoughnessTexture（roughness 在 G、metallic 在 B）。
    MetallicRoughness,
    /// glTF occlusionTexture（读 R）。
    Occlusion,
    /// glTF baseColorTexture。
    BaseColor,
    /// glTF emissiveTexture。
    Emissive,
}

impl TextureRole {
    /// 角色 → BCn 目标格式（sRGB 语义随角色定：颜色类 sRGB、数据类线性）。
    /// `None` = 该角色不压缩，调用方走 RGBA8 现行路径。
    #[must_use]
    pub fn target_format(self, has_alpha: bool) -> Option<vk::Format> {
        match self {
            Self::Normal => Some(vk::Format::BC5_UNORM_BLOCK),
            Self::Occlusion => Some(vk::Format::BC4_UNORM_BLOCK),
            Self::MetallicRoughness => None,
            Self::BaseColor | Self::Emissive => Some(if has_alpha {
                vk::Format::BC3_SRGB_BLOCK
            } else {
                vk::Format::BC1_RGB_SRGB_BLOCK
            }),
        }
    }
}

/// 烘焙拒绝：不影响帧循环，调用方 warn 后按 RGBA8 现行路径兜底。
#[derive(Debug, Error)]
pub enum BakeError {
    #[error("角色 {0:?} 不压缩，不应进烘焙")]
    NotCompressible(TextureRole),
    #[error("贴图像素数据不足：需要 {need}B，实际 {got}B")]
    NotEnoughPixels { need: usize, got: usize },
}

/// 一次烘焙的产物：各级块数据连排，offset 按 [`level_bytes`] 累计即得。
#[derive(Debug)]
pub struct BakedBcn {
    pub format: vk::Format,
    /// level-0 宽高（已补到 4 的倍数，建 VkImage 按这对）。
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub data: Vec<u8>,
}

impl BakedBcn {
    /// 某级 mip 的压缩字节数。
    #[must_use]
    pub fn level_bytes(&self, level: u32) -> u64 {
        level_bytes(self.format, self.width >> level, self.height >> level)
    }

    /// 全链字节合计（= staging 连排的源长度）。
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        (0..self.mip_levels).map(|l| self.level_bytes(l)).sum()
    }
}

/// alpha 全 255 探测（BC1 只有 1 位二值 alpha，有插值 alpha 就得 BC3）。
#[must_use]
pub fn has_alpha(rgba: &[u8]) -> bool {
    rgba.iter().skip(3).step_by(4).any(|&a| a != 255)
}

/// 烘焙：alpha 探测 → 选格式 → 源图补到 4 的倍数 → box-filter 逐级减半并逐级编码。
///
/// # Errors
/// 角色不压缩、像素数据不足。
pub fn bake(
    role: TextureRole,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<BakedBcn, BakeError> {
    let started = std::time::Instant::now();
    let Some(format) = role.target_format(has_alpha(rgba)) else {
        return Err(BakeError::NotCompressible(role));
    };
    let need = usize::checked_mul(width as usize, height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or(BakeError::NotEnoughPixels {
            need: usize::MAX,
            got: rgba.len(),
        })?;
    if rgba.len() < need {
        return Err(BakeError::NotEnoughPixels {
            need,
            got: rgba.len(),
        });
    }
    // 补到 4 的倍数（level-0 红线），mip 链从补后尺寸展开
    let (mut data, mut w, mut h) = pad_rgba8(rgba, width, height);
    let mip_levels = full_mip_chain(w, h);
    // level-0 尺寸先钉住：循环里 w/h 会一路减到末级
    let (level0_w, level0_h) = (w, h);
    let (enc, params) = encoder_for(format);
    let mut out = Vec::with_capacity(
        (0..mip_levels)
            .map(|l| level_bytes(format, w >> l, h >> l))
            .sum::<u64>() as usize,
    );
    for level in 0..mip_levels {
        if level > 0 {
            (data, w, h) = downsample_rgba8(&data, w, h);
        }
        let start = out.len();
        out.resize(start + enc.compressed_size(w as usize, h as usize), 0);
        enc.compress(&data, w as usize, h as usize, params, &mut out[start..]);
    }
    info!(
        "BCn 烘焙: {src_w}×{src_h} → {w}×{h} {format:?} {mip_levels} 级, \
        源 {src}B → 块数据 {dst}B, 耗时 {ms}ms",
        src_w = width,
        src_h = height,
        w = level0_w,
        h = level0_h,
        src = rgba.len(),
        dst = out.len(),
        ms = started.elapsed().as_millis(),
    );
    Ok(BakedBcn {
        format,
        width: level0_w,
        height: level0_h,
        mip_levels,
        data: out,
    })
}

/// 编码格式与参数（vk 格式 → texpresso 格式）。
/// 算法用 RangeFit（快摄合档）：实测 1024² 单张 16ms vs ClusterFit 的 2.8s，
/// 运行时烘焙承受不了离线级质量档的时长；质量即 stb_dxt 档，为游戏行业
/// 广泛使用的基线。主角材质渐变条带风险由截图验收把关。
fn encoder_for(format: vk::Format) -> (texpresso::Format, texpresso::Params) {
    let enc = match format {
        vk::Format::BC1_RGB_UNORM_BLOCK
        | vk::Format::BC1_RGB_SRGB_BLOCK
        | vk::Format::BC1_RGBA_UNORM_BLOCK
        | vk::Format::BC1_RGBA_SRGB_BLOCK => texpresso::Format::Bc1,
        vk::Format::BC2_UNORM_BLOCK | vk::Format::BC2_SRGB_BLOCK => texpresso::Format::Bc2,
        vk::Format::BC3_UNORM_BLOCK | vk::Format::BC3_SRGB_BLOCK => texpresso::Format::Bc3,
        vk::Format::BC4_UNORM_BLOCK => texpresso::Format::Bc4,
        vk::Format::BC5_UNORM_BLOCK => texpresso::Format::Bc5,
        _ => unreachable!("只允许对 target_format 放行的 BCn 格式调用"),
    };
    // RangeFit 快摄合：运行时烘焙的时长/质量取舍，见函数头注释
    (
        enc,
        texpresso::Params {
            algorithm: texpresso::Algorithm::RangeFit,
            ..texpresso::Params::default()
        },
    )
}

/// 边缘扩展到 4 的倍数（Vulkan 压缩格式 level-0 宽高红线）。UV 0..1 语义随补边
/// 有半 texel 内的微移，属压缩档位预期成本。
#[must_use]
fn pad_rgba8(rgba: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let pw = width.div_ceil(4) * 4;
    let ph = height.div_ceil(4) * 4;
    if pw == width && ph == height {
        return (rgba.to_vec(), width, height);
    }
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        let sy = y.min(height - 1) as usize;
        for x in 0..pw {
            let sx = x.min(width - 1) as usize;
            let src = (sy * width as usize + sx) * 4;
            let dst = (y as usize * pw as usize + x as usize) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    (out, pw, ph)
}

/// 2×2 box filter 减半。直接平均 sRGB 字节省 gamma 往返，mip 高级与 GPU 线性域
/// 过滤有细微色差，属压缩档位预期成本（视觉验收把关）。
#[must_use]
fn downsample_rgba8(src: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let w = (width / 2).max(1);
    let h = (height / 2).max(1);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            // 2×2 邻域，越界侧贴回边界（奇数尺寸末级）
            let xs = [x * 2, (x * 2 + 1).min(width - 1)];
            let ys = [y * 2, (y * 2 + 1).min(height - 1)];
            let dst = (y as usize * w as usize + x as usize) * 4;
            for c in 0..4 {
                let sum: u32 = ys
                    .iter()
                    .flat_map(|&sy| xs.iter().map(move |&sx| (sy, sx)))
                    .map(|(sy, sx)| {
                        src[(sy as usize * width as usize + sx as usize) * 4 + c] as u32
                    })
                    .sum();
                out[dst + c] = (sum / 4) as u8;
            }
        }
    }
    (out, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::core::device::transfer::block_bytes;

    #[test]
    fn block_byte_table() {
        assert_eq!(block_bytes(vk::Format::BC1_RGB_UNORM_BLOCK), 8);
        assert_eq!(block_bytes(vk::Format::BC1_RGBA_SRGB_BLOCK), 8);
        assert_eq!(block_bytes(vk::Format::BC4_UNORM_BLOCK), 8);
        assert_eq!(block_bytes(vk::Format::BC3_SRGB_BLOCK), 16);
        assert_eq!(block_bytes(vk::Format::BC5_UNORM_BLOCK), 16);
        assert_eq!(block_bytes(vk::Format::R8G8B8A8_UNORM), 0);
    }

    #[test]
    fn level_byte_math() {
        assert_eq!(level_bytes(vk::Format::BC1_RGB_SRGB_BLOCK, 4, 4), 8);
        assert_eq!(level_bytes(vk::Format::BC1_RGB_SRGB_BLOCK, 2, 2), 8);
        assert_eq!(level_bytes(vk::Format::BC1_RGB_SRGB_BLOCK, 8, 6), 32);
        assert_eq!(level_bytes(vk::Format::BC5_UNORM_BLOCK, 8, 8), 64);
        assert_eq!(level_bytes(vk::Format::BC3_SRGB_BLOCK, 6, 6), 64);
    }

    #[test]
    fn baked_chain_offsets_stay_block_aligned() {
        // 连排各级 offset 必须是块字节数的倍数（bufferOffset 红线）
        let baked = bake(TextureRole::BaseColor, &solid(8, 6, 200, 60, 10), 8, 6).unwrap();
        assert_eq!(baked.format, vk::Format::BC1_RGB_SRGB_BLOCK);
        let mut offset = 0u64;
        for level in 0..baked.mip_levels {
            assert_eq!(offset % 8, 0);
            offset += baked.level_bytes(level);
        }
        assert_eq!(offset, baked.data.len() as u64);
    }

    #[test]
    fn alpha_probe_switches_color_format() {
        assert!(!has_alpha(&solid(4, 4, 10, 20, 30)));
        let mut rgba = solid(4, 4, 10, 20, 30);
        rgba[7] = 128;
        assert!(has_alpha(&rgba));
        let baked = bake(TextureRole::BaseColor, &rgba, 4, 4).unwrap();
        assert_eq!(baked.format, vk::Format::BC3_SRGB_BLOCK);
    }

    #[test]
    fn pad_extends_edges() {
        let (out, w, h) = pad_rgba8(&solid(6, 4, 9, 8, 7), 6, 4);
        assert_eq!((w, h), (8, 4));
        let px = |x: u32, y: u32| (y as usize * 8 + x as usize) * 4;
        assert_eq!(&out[px(0, 0)..px(0, 0) + 3], &[9, 8, 7]);
        assert_eq!(&out[px(6, 0)..px(6, 0) + 3], &[9, 8, 7]); // 贴回 x=5
        assert_eq!(&out[px(5, 0)..px(5, 0) + 3], &[9, 8, 7]);
    }

    #[test]
    fn downsample_averages_block() {
        let (out, w, h) = downsample_rgba8(&solid(2, 2, 100, 50, 0), 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[..4], &[100, 50, 0, 255]);
    }

    #[test]
    fn bake_round_trips_solid_color() {
        let baked = bake(TextureRole::BaseColor, &solid(4, 4, 200, 60, 10), 4, 4).unwrap();
        assert_eq!(baked.mip_levels, 3); // 4×4 → 4/2/1 全链
        assert_eq!((baked.width, baked.height), (4, 4));
        let px = texpresso::Format::Bc1.decompress_block(&baked.data[..8]);
        for p in px {
            assert!(p[0] > 190 && p[1] < 70 && p[2] < 30, "解块 {p:?}");
            assert_eq!(p[3], 255);
        }
    }

    #[test]
    fn bake_round_trips_bc5_channels() {
        let mut rgba = solid(4, 4, 200, 100, 0);
        for p in rgba.chunks_mut(4) {
            p[0] = 200;
            p[1] = 100;
            p[2] = 0;
        }
        let baked = bake(TextureRole::Normal, &rgba, 4, 4).unwrap();
        assert_eq!(baked.format, vk::Format::BC5_UNORM_BLOCK);
        let px = texpresso::Format::Bc5.decompress_block(&baked.data[..16]);
        for p in px {
            assert!((p[0] as i32 - 200).abs() <= 4, "R 通道 {p:?}");
            assert!((p[1] as i32 - 100).abs() <= 4, "G 通道 {p:?}");
        }
    }

    #[test]
    fn bake_rejects_non_compressible_role() {
        let err = bake(TextureRole::MetallicRoughness, &solid(4, 4, 0, 0, 0), 4, 4).unwrap_err();
        assert!(matches!(err, BakeError::NotCompressible(_)));
    }

    fn solid(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        vec![r, g, b, 255]
            .into_iter()
            .cycle()
            .take((width * height * 4) as usize)
            .collect()
    }
}
