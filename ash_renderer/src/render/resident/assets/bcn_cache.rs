//! BCn 烘焙缓存：解码后 RGBA8 内容哈希为键的磁盘缓存（`assets/bcn_cache/`）。
//!
//!  = 哈希(烘焙版本 + 角色 + 宽高 + mip0 像素字节)。烘焙与查找发生在同一进程、
//! 同一解码器，无跨工具哈希不一致；glb 内嵌贴图与 .gltf 外链贴图因此同一套键。
//! 命中直读块数据，未命中现烤后写回——只付一次编码成本。文件损坏/版本不符按
//! 缺席处理（重烤覆盖），不阻塞加载。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;

use ash::vk;
use bevy::log::warn;

use super::bcn::{BakedBcn, TextureRole, BAKE_VERSION};
use crate::render::core::device::transfer::level_bytes;

/// 缓存目录（相对运行目录；与资产根同侧，gitignore 挡住不入库）。
const CACHE_DIR: &str = "assets/bcn_cache";
/// 文件魔数：解析拒绝时按缓存缺失处理。
const MAGIC: &[u8; 8] = b"ASHBCN1\n";
/// 头部长度：magic + 烘焙版本 + vk 格式 + 宽 + 高 + 级数。
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + 4 + 4;

/// 缓存键：烘焙版本、角色、宽高、mip0 字节全进哈希域。
#[must_use]
pub fn cache_key(role: TextureRole, rgba: &[u8], width: u32, height: u32) -> u64 {
    let mut h = DefaultHasher::new();
    BAKE_VERSION.hash(&mut h);
    role.hash(&mut h);
    width.hash(&mut h);
    height.hash(&mut h);
    h.write(rgba);
    h.finish()
}

/// BCn 烘焙缓存（目录懒建）。
pub struct BcnCache {
    dir: PathBuf,
}

impl Default for BcnCache {
    fn default() -> Self {
        Self::open()
    }
}

impl BcnCache {
    /// 打开缓存目录（不存在则建；建失败时读写各调用点按缺失处理）。
    #[must_use]
    pub fn open() -> Self {
        let dir = std::path::Path::new(CACHE_DIR);
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!("BCn 缓存目录 {CACHE_DIR} 创建失败({e}),本次全程现烤不落盘");
        }
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn file_path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}.bcn"))
    }

    /// 读缓存。文件缺失、魔数/版本/长度不符一律返回 None（当缺失处理，重烤覆盖）。
    pub fn load(&self, key: u64) -> Option<BakedBcn> {
        let bytes = std::fs::read(self.file_path(key)).ok()?;
        match cache_from_bytes(&bytes) {
            Ok(baked) => Some(baked),
            Err(e) => {
                warn!("BCn 存 {key:016x} 读取无效({e}),按缺失重烤");
                None
            }
        }
    }

    /// 写缓存：temp + rename 半截文件；失败只 warn（缓存是优化不是正确性）。
    ///
    /// # Errors
    /// 目录写失败。
    pub fn store(&self, key: u64, baked: &BakedBcn) -> io::Result<()> {
        let tmp = self.dir.join(format!("{key:016x}.tmp"));
        std::fs::write(&tmp, cache_to_bytes(baked))?;
        std::fs::rename(&tmp, self.file_path(key))?;
        Ok(())
    }
}

/// 缓存文件布局：`magic | 版本u32 | vk格式u32 | 宽u32 | 高u32 | 级数u32 | 各级块数据连排`。
fn cache_to_bytes(baked: &BakedBcn) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + baked.data.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&BAKE_VERSION.to_le_bytes());
    out.extend_from_slice(&baked.format.as_raw().to_le_bytes());
    out.extend_from_slice(&baked.width.to_le_bytes());
    out.extend_from_slice(&baked.height.to_le_bytes());
    out.extend_from_slice(&baked.mip_levels.to_le_bytes());
    out.extend_from_slice(&baked.data);
    out
}

/// 解码缓存文件；长度与块算术对不上视为损坏。
fn cache_from_bytes(bytes: &[u8]) -> Result<BakedBcn, String> {
    if bytes.len() < HEADER_LEN {
        return Err(format!("连 {HEADER_LEN}B 都不够,实际 {}B", bytes.len()));
    }
    if &bytes[..8] != MAGIC {
        return Err("魔数不符".into());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("4B"));
    if version != BAKE_VERSION {
        return Err(format!("烘焙版本 {version} ≠ 当前 {BAKE_VERSION}"));
    }
    let format = vk::Format::from_raw(i32::from_le_bytes(bytes[12..16].try_into().expect("4B")));
    let width = u32::from_le_bytes(bytes[16..20].try_into().expect("4B"));
    let height = u32::from_le_bytes(bytes[20..24].try_into().expect("4B"));
    let mip_levels = u32::from_le_bytes(bytes[24..28].try_into().expect("4B"));
    let data = &bytes[HEADER_LEN..];
    let expect: u64 = (0..mip_levels)
        .map(|l| level_bytes(format, width >> l, height >> l))
        .sum();
    if data.len() as u64 != expect {
        return Err(format!("块数据 {}B ≠ 全链 {expect}B", data.len()));
    }
    Ok(BakedBcn {
        format,
        width,
        height,
        mip_levels,
        data: data.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::resident::assets::bcn;

    fn solid(width: u32, height: u32) -> Vec<u8> {
        vec![10, 20, 30, 255]
            .into_iter()
            .cycle()
            .take((width * height * 4) as usize)
            .collect()
    }

    fn baked_4x4() -> BakedBcn {
        bcn::bake(TextureRole::BaseColor, &solid(4, 4), 4, 4).unwrap()
    }

    #[test]
    fn cache_round_trip() {
        let baked = baked_4x4();
        let back = cache_from_bytes(&cache_to_bytes(&baked)).unwrap();
        assert_eq!((back.width, back.height, back.mip_levels), (4, 4, 3));
        assert_eq!(back.format, baked.format);
        assert_eq!(back.data, baked.data);
    }

    #[test]
    fn cache_rejects_truncated_and_foreign() {
        let bytes = cache_to_bytes(&baked_4x4());
        assert!(cache_from_bytes(&bytes[..bytes.len() - 1]).is_err());
        let mut foreign = bytes;
        foreign[8] = 0xFF; // 版本字段改写 → 版本不符
        assert!(cache_from_bytes(&foreign).is_err());
    }

    #[test]
    fn cache_key_separates_role_and_content() {
        let a = cache_key(TextureRole::BaseColor, &solid(4, 4), 4, 4);
        let b = cache_key(TextureRole::Normal, &solid(4, 4), 4, 4);
        let c = cache_key(TextureRole::BaseColor, &solid(4, 8), 4, 8);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, cache_key(TextureRole::BaseColor, &solid(4, 4), 4, 4));
    }
}
