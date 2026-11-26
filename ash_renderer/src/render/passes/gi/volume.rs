//! 格子系统 CPU 镜像（9.5.1）：与 `shaders/common/gi_cell.wgsl` 同一组公式——体素尺寸、
//! 球判据、级联归属、体积坐标、存储槽、cell 线性化与心跳距离。宿主侧只做两件事：
//! 每帧算各级整数体素偏移（决定滚动是否触发）、给单元测试与 BRP 对账当参照。
//!
//! 两份实现必须逐式一致（同一取整语义、同一有符号正模）；改一处必须同步另一处。
//! 口径推导见 9.5 施工计划 §0.1 与讲解《patch 池与格子系统讲解》。

use bevy::math::{IVec3, Mat4, Vec2, Vec3, Vec4};

use crate::render::passes::ibl::octahedral;

/// 方向桶网格边长（16 桶）。
pub const BUCKET_RESOLUTION: u32 = 4;
/// 每个 cell 的桶数。
pub const BUCKETS_PER_CELL: u32 = BUCKET_RESOLUTION * BUCKET_RESOLUTION;
/// 级联数上限（GI UBO 的偏移数组长度，与 WGSL `MAX_CASCADES` 同值）。
pub const MAX_CASCADES: usize = 8;
/// 空 patch 哨兵（cell→patch 与 patch→cell 双向同值）。
pub const INVALID_PATCH: u32 = u32::MAX;

/// Volume 规格：最细级体素边长、各级 res、级联数、patch 池容量。
/// `Size`（最粗级立方外沿）只从 [`Self::size`] 派生一处，不在别处再写常量。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceCacheConfig {
    /// 最细级体素边长（米）。
    pub voxel_min: f32,
    /// 每级每轴体素数（立方，Res³ 个体素 × 16 桶 × 级联数 = cell 总数）。
    pub res: u32,
    /// 级联数（1..=8）。
    pub cascades: u32,
    /// patch 池容量（2 的幂，且整除 2³²——环形取模前提）。
    pub capacity: u32,
}

impl SurfaceCacheConfig {
    /// 起步档：Res 32 / 4 级 / Size 128 m / 池 65536（与 Unity 默认体积同档）。
    pub const fn default() -> Self {
        Self {
            voxel_min: 0.5,
            res: 32,
            cascades: 4,
            capacity: 65_536,
        }
    }

    /// 由最粗级立方外沿（Size，米）反推最细级体素：`voxel_min = Size/(Res·2^(C−1))`。
    pub fn from_size(size: f32, res: u32, cascades: u32, capacity: u32) -> Self {
        let levels = 1u32 << cascades.saturating_sub(1).min(31);
        Self {
            voxel_min: size / (res as f32 * levels as f32),
            res,
            cascades,
            capacity,
        }
    }

    /// 最粗级立方外沿（米）= `voxel_min · res · 2^(C−1)`；起步档 0.5·32·8 = 128 m。
    pub fn size(&self) -> f32 {
        let levels = 1u64 << self.cascades.saturating_sub(1).min(63);
        self.voxel_min * self.res as f32 * levels as f32
    }

    /// cell 总数（含全部级联与桶）。
    pub fn cell_count(&self) -> u64 {
        u64::from(self.res).pow(3) * u64::from(BUCKETS_PER_CELL) * u64::from(self.cascades)
    }

    /// 规格自检：返回问题清单（空 = 通过）。BRP `gi_set` 与建表前共用。
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !(self.voxel_min.is_finite() && self.voxel_min > 0.0) {
            out.push("voxel_min 必须是正有限数".to_owned());
        }
        if self.res == 0 {
            out.push("res 必须大于 0".to_owned());
        }
        if !(1..=MAX_CASCADES as u32).contains(&self.cascades) {
            out.push(format!("级联数须在 1..={MAX_CASCADES}"));
        }
        // 环形取模前提：容量是 2 的幂才整除 2³²（65536 / 2^18 都满足）
        if self.capacity == 0 || !self.capacity.is_power_of_two() {
            out.push("池容量必须是 2 的幂".to_owned());
        }
        out
    }
}

/// 有符号正模：WGSL 的 `%` 对负输入保留符号，存储槽要求落在 `[0, m)`。
pub fn pos_mod(a: i32, m: i32) -> i32 {
    let r = a % m;
    if r >= 0 {
        r
    } else {
        r + m
    }
}

/// 第 `cascade` 级体素边长 = `voxel_min · 2^cascade`。
pub fn voxel_size(voxel_min: f32, cascade: u32) -> f32 {
    voxel_min * (1u64 << cascade.min(63)) as f32
}

/// 球判据：位置落在该级内 = 到 Volume 中心距离 < `voxel·(res/2 − 0.5)`。
pub fn is_inside_cascade(center: Vec3, p: Vec3, voxel: f32, res: u32) -> bool {
    let radius = voxel * (res as f32 * 0.5 - 0.5);
    (p - center).length_squared() < radius * radius
}

/// 从最细级起找第一个满足球判据的级联；都不满足返回 None。
pub fn resolve_cascade(
    center: Vec3,
    p: Vec3,
    voxel_min: f32,
    res: u32,
    cascades: u32,
) -> Option<u32> {
    (0..cascades).find(|&i| is_inside_cascade(center, p, voxel_size(voxel_min, i), res))
}

/// 世界位置 → 体积坐标（clamp 到 `[0, res)`）：`pv = int(p/v − offset + res/2)`。
pub fn volume_position(p: Vec3, voxel: f32, offset: IVec3, res: i32) -> IVec3 {
    let v = (p / voxel).floor().as_ivec3() - offset + IVec3::splat(res / 2);
    v.clamp(IVec3::ZERO, IVec3::splat(res - 1))
}

/// 体积坐标 → 存储槽：`storage = (pv + offset) mod res`。代进去 offset 会消掉
///（`storage = (floor(p/v) + res/2) mod res`）——世界体素到存储槽的映射与相机无关。
pub fn volume_to_storage(pv: IVec3, offset: IVec3, res: i32) -> IVec3 {
    IVec3::new(
        pos_mod(pv.x + offset.x, res),
        pos_mod(pv.y + offset.y, res),
        pos_mod(pv.z + offset.z, res),
    )
}

/// 存储槽 + 当前偏移 → 该槽代表的体积坐标：`pv = (storage − offset) mod res`。
pub fn storage_to_volume(storage: IVec3, offset: IVec3, res: i32) -> IVec3 {
    IVec3::new(
        pos_mod(storage.x - offset.x, res),
        pos_mod(storage.y - offset.y, res),
        pos_mod(storage.z - offset.z, res),
    )
}

/// 体积坐标在体积空间里的世界位置（体素中心对齐）：`(pv − res/2 + 0.5 + offset)·v`。
pub fn volume_world_center(pv: IVec3, offset: IVec3, voxel: f32, res: i32) -> Vec3 {
    (pv.as_vec3() - Vec3::splat(res as f32 * 0.5) + Vec3::splat(0.5) + offset.as_vec3()) * voxel
}

/// cell 线性化：`cascadeBase + 16·(x·res² + y·res + z) + bucket`。
pub fn cell_index(cascade: u32, storage: IVec3, bucket: u32, res: u32) -> u32 {
    let plane = (storage.x as u32) * res * res + (storage.y as u32) * res + (storage.z as u32);
    cascade * (res * res * res * BUCKETS_PER_CELL) + BUCKETS_PER_CELL * plane + bucket
}

/// cell → 级联。
pub fn cell_cascade(cell: u32, res: u32) -> u32 {
    cell / (res * res * res * BUCKETS_PER_CELL)
}

/// cell → 存储槽（xyz）。
pub fn cell_storage(cell: u32, res: u32) -> IVec3 {
    let plane = (cell % (res * res * res * BUCKETS_PER_CELL)) / BUCKETS_PER_CELL;
    IVec3::new(
        (plane / (res * res)) as i32,
        ((plane / res) % res) as i32,
        (plane % res) as i32,
    )
}

/// cell → 方向桶。
pub fn cell_bucket(cell: u32) -> u32 {
    cell % BUCKETS_PER_CELL
}

/// 心跳/帧号回绕距离（16 位模）。
pub fn heartbeat_age(frame_index: u32, heartbeat: u32) -> u32 {
    (frame_index.wrapping_sub(heartbeat)) & 0xFFFF
}

/// 方向 → 桶号（复用 octahedral 的 4×4 映射，不另写）。
pub fn direction_bucket(normal: [f32; 3]) -> u32 {
    octahedral::direction_to_bucket(normal, BUCKET_RESOLUTION)
}

/// 与 `gi_cell.wgsl` 同式的 reverse-Z 深度反投影。
pub(crate) fn world_from_depth(uv: Vec2, depth: f32, inv_view_proj: Mat4) -> Vec3 {
    let world = inv_view_proj * Vec4::new(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    world.truncate() / world.w
}

/// 深度法线 CPU 镜像；depth 顺序为 center/left/right/down/up。
/// 仅视口边缘允许内侧单边差分，内部断层与退化平面返回零。
pub(crate) fn flat_normal(
    uv: Vec2,
    texel: Vec2,
    depth: [f32; 5],
    inv_view_proj: Mat4,
    camera_pos: Vec3,
) -> Vec3 {
    let [d0, left, right, down, up] = depth;
    let ok = |d: f32| d > 0.0 && (d - d0).abs() <= 0.05 * d0;
    let world = |uv: Vec2, d: f32| world_from_depth(uv, d, inv_view_proj);
    let p = world(uv, d0);
    let dx = Vec2::new(texel.x, 0.0);
    let dy = Vec2::new(0.0, texel.y);
    let t = if uv.x >= texel.x && uv.x <= 1.0 - texel.x && ok(left) && ok(right) {
        world(uv + dx, right) - world(uv - dx, left)
    } else if uv.x < texel.x && uv.x + texel.x <= 1.0 && ok(right) {
        world(uv + dx, right) - p
    } else if uv.x > 1.0 - texel.x && uv.x - texel.x >= 0.0 && ok(left) {
        p - world(uv - dx, left)
    } else {
        Vec3::ZERO
    };
    let b = if uv.y >= texel.y && uv.y <= 1.0 - texel.y && ok(up) && ok(down) {
        world(uv + dy, down) - world(uv - dy, up)
    } else if uv.y < texel.y && uv.y + texel.y <= 1.0 && ok(down) {
        world(uv + dy, down) - p
    } else if uv.y > 1.0 - texel.y && uv.y - texel.y >= 0.0 && ok(up) {
        p - world(uv - dy, up)
    } else {
        Vec3::ZERO
    };
    let cross = t.cross(b);
    if t.length_squared() < 1e-12 || b.length_squared() < 1e-12 || cross.length_squared() < 1e-24 {
        return Vec3::ZERO;
    }
    let n = cross.normalize();
    if n.dot(camera_pos - p) < 0.0 {
        -n
    } else {
        n
    }
}

/// 播种与 9.7 屏幕 gather 共用的 1/4 网格口径：本帧在 4×4 块内被播种的子像素（列, 行）。
///
/// WGSL 同式见 `shaders/common/gi_lookup.wgsl` 的 `seed_subsample`：
/// `sub = (frame·7) mod 16`，列 = `sub % 4`、行 = `sub / 4`。7 与 16 互质 → 16 帧走遍
/// 整块且是双射（不是来回两点）。改动必须两边同步，否则 gather 的命中率读数不可信。
#[allow(dead_code)] // 9.7.4 的 screen_gather 组装 push 时接入，本任务先落实现与单测
pub fn seed_subsample(frame_index: u32) -> (u32, u32) {
    let sub = frame_index.wrapping_mul(7) % 16;
    (sub % 4, sub / 4)
}

/// 1/4 像素 + 块内偏移 → 全分辨率像素（与 `patch_alloc.wgsl` 的
/// `full = low*4 + sub%4 / sub/4` 同式；两轴各自线性放大）。
#[allow(dead_code)] // 同上：9.7.4 起被 screen_gather / 上采样口径复用
pub fn low_to_full(low: (u32, u32), sub: (u32, u32)) -> (u32, u32) {
    (low.0 * 4 + sub.0, low.1 * 4 + sub.1)
}

/// gather 的 3 个块内样本点：本帧播种点、它的对角、块心 `(1, 1)`。
///
/// 三点都落在同一个 4×4 块内（9.7 施工计划 §9.7.1）。块心取四个中心像素的左上那个，
/// 与 `low_to_full` 的整数算术一致。
#[allow(dead_code)] // 同上：9.7.4 的 3 点采样口径
pub fn gather_sample_offsets(frame_index: u32) -> [(u32, u32); 3] {
    let (sx, sy) = seed_subsample(frame_index);
    [(sx, sy), (3 - sx, 3 - sy), (1, 1)]
}

/// 各级整数体素偏移：`offset_i = floor(focus / v_i + 0.5)`（按整个体素对齐；
/// 相机在体素内部游移不改变偏移，跨过体素边界才变 1）。
pub fn cascade_offsets(focus: Vec3, config: &SurfaceCacheConfig) -> [IVec3; MAX_CASCADES] {
    let mut out = [IVec3::ZERO; MAX_CASCADES];
    for (i, slot) in out.iter_mut().enumerate() {
        if i as u32 >= config.cascades {
            break;
        }
        let voxel = voxel_size(config.voxel_min, i as u32);
        *slot = (focus / voxel + Vec3::splat(0.5)).floor().as_ivec3();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: SurfaceCacheConfig = SurfaceCacheConfig::default();

    #[test]
    fn depth_reconstruction_uses_vulkan_uv_y_and_faces_camera() {
        let p = world_from_depth(Vec2::new(0.25, 0.75), 0.4, Mat4::IDENTITY);
        assert!((p - Vec3::new(-0.5, -0.5, 0.4)).length() < 1e-6);
        let n = flat_normal(
            Vec2::splat(0.5),
            Vec2::splat(0.05),
            [0.4; 5],
            Mat4::IDENTITY,
            Vec3::Z,
        );
        assert!((n - Vec3::Z).length() < 1e-6);
        let n = flat_normal(
            Vec2::splat(0.5),
            Vec2::splat(0.05),
            [0.4; 5],
            Mat4::IDENTITY,
            -Vec3::Z,
        );
        assert!((n + Vec3::Z).length() < 1e-6);
    }

    #[test]
    fn depth_stencil_rejects_interior_edges_but_preserves_viewport_boundary() {
        let depth = [0.4, 0.0, 0.4, 0.4, 0.4];
        assert_eq!(
            flat_normal(
                Vec2::splat(0.5),
                Vec2::splat(0.05),
                depth,
                Mat4::IDENTITY,
                Vec3::Z
            ),
            Vec3::ZERO
        );
        let edge = flat_normal(
            Vec2::new(0.025, 0.5),
            Vec2::splat(0.05),
            depth,
            Mat4::IDENTITY,
            Vec3::Z,
        );
        assert!((edge - Vec3::Z).length() < 1e-6);
        let jump = [0.4, 0.6, 0.4, 0.4, 0.4];
        assert_eq!(
            flat_normal(
                Vec2::splat(0.5),
                Vec2::splat(0.05),
                jump,
                Mat4::IDENTITY,
                Vec3::Z
            ),
            Vec3::ZERO
        );
        let collapsed = Mat4::from_scale(Vec3::new(0.0, 1.0, 1.0));
        assert_eq!(
            flat_normal(
                Vec2::splat(0.5),
                Vec2::splat(0.05),
                [0.4; 5],
                collapsed,
                Vec3::Z
            ),
            Vec3::ZERO
        );
    }

    /// 正模：负输入的存储槽必须落在 [0, res)。
    #[test]
    fn pos_mod_is_positive_for_negative_inputs() {
        assert_eq!(pos_mod(-1, 32), 31);
        assert_eq!(pos_mod(-33, 32), 31);
        assert_eq!(pos_mod(0, 32), 0);
        assert_eq!(pos_mod(31, 32), 31);
        assert_eq!(pos_mod(32, 32), 0);
        for a in -100..100 {
            let r = pos_mod(a, 32);
            assert!((0..32).contains(&r), "pos_mod({a}) = {r}");
        }
    }

    /// 规格派生：Size 与 voxel_min 互逆；cell 总数 = 2^21（起步档 210 万）。
    #[test]
    fn config_size_and_cell_count() {
        assert!((CONFIG.size() - 128.0).abs() < 1e-6);
        let derived = SurfaceCacheConfig::from_size(128.0, 32, 4, 65_536);
        assert!((derived.voxel_min - 0.5).abs() < 1e-6);
        assert_eq!(CONFIG.cell_count(), 2_097_152);
        assert!(CONFIG.problems().is_empty());
        assert!(!SurfaceCacheConfig {
            capacity: 65_535,
            ..CONFIG
        }
        .problems()
        .is_empty());
    }

    /// 讲解里的具体样本：相机 (10.3,1.7,−4.2)、样本 (10.6,1.7,−4.0)、法线朝上 →
    /// pv (16,16,16)、storage (5,19,8)、桶 14、cell 91,790。
    #[test]
    fn walkthrough_sample_matches_doc() {
        let focus = Vec3::new(10.3, 1.7, -4.2);
        let p = Vec3::new(10.6, 1.7, -4.0);
        let offsets = cascade_offsets(focus, &CONFIG);
        assert_eq!(offsets[0], IVec3::new(21, 3, -8));
        let cascade = resolve_cascade(focus, p, CONFIG.voxel_min, CONFIG.res, CONFIG.cascades)
            .expect("样本在第一级球内");
        assert_eq!(cascade, 0);
        let voxel = voxel_size(CONFIG.voxel_min, cascade);
        let pv = volume_position(p, voxel, offsets[0], CONFIG.res as i32);
        assert_eq!(pv, IVec3::new(16, 16, 16));
        let storage = volume_to_storage(pv, offsets[0], CONFIG.res as i32);
        assert_eq!(storage, IVec3::new(5, 19, 8));
        let bucket = direction_bucket([0.0, 1.0, 0.0]);
        assert_eq!(bucket, 14);
        assert_eq!(cell_index(cascade, storage, bucket, CONFIG.res), 91_790);
    }

    /// 存储槽与世界位置无关：同一世界体素在相机移动后落到同一槽。
    #[test]
    fn storage_is_world_aligned() {
        let res = CONFIG.res as i32;
        let p = Vec3::new(3.2, 1.6, -7.9);
        let mut seen = None;
        for shift in [
            Vec3::ZERO,
            Vec3::new(0.4, 0.0, 0.0),
            Vec3::new(-0.45, 0.3, 0.2),
        ] {
            let focus = Vec3::new(1.0, 1.0, 1.0) + shift;
            let offsets = cascade_offsets(focus, &CONFIG);
            let voxel = voxel_size(CONFIG.voxel_min, 0);
            let pv = volume_position(p, voxel, offsets[0], res);
            let storage = volume_to_storage(pv, offsets[0], res);
            if let Some(prev) = seen {
                assert_eq!(storage, prev, "同一世界体素的存储槽随相机变了");
            }
            seen = Some(storage);
        }
    }

    /// cell 线性化往返：任意 (级联, 存储槽, 桶) 解回自身。
    #[test]
    fn cell_index_round_trips() {
        let res = CONFIG.res;
        for cascade in 0..CONFIG.cascades {
            for storage in [
                IVec3::new(0, 0, 0),
                IVec3::new(31, 31, 31),
                IVec3::new(5, 19, 8),
                IVec3::new(31, 0, 17),
            ] {
                for bucket in 0..BUCKETS_PER_CELL {
                    let cell = cell_index(cascade, storage, bucket, res);
                    assert_eq!(cell_cascade(cell, res), cascade);
                    assert_eq!(cell_storage(cell, res), storage);
                    assert_eq!(cell_bucket(cell), bucket);
                }
            }
        }
        assert!(cell_index(CONFIG.cascades, IVec3::ZERO, 0, res) >= CONFIG.cell_count() as u32);
    }

    /// 桶中心方向 ← octahedral 镜像 → 桶号：16 桶互逆（播种与查询同一来源的前提）。
    #[test]
    fn bucket_centers_round_trip() {
        for bucket in 0..BUCKETS_PER_CELL {
            let dir = octahedral::bucket_to_direction(bucket, BUCKET_RESOLUTION);
            assert_eq!(
                direction_bucket(dir),
                bucket,
                "桶 {bucket} 中心 {dir:?} 落回了别的桶"
            );
        }
    }

    /// 级联偏移只随体素边界变：0.4 m 步进不倒退、每步变化不超过 1 个体素。
    #[test]
    fn cascade_offsets_quantize_and_move_by_at_most_one() {
        let mut prev = cascade_offsets(Vec3::ZERO, &CONFIG);
        let mut focus = Vec3::ZERO;
        for _ in 0..32 {
            focus.x += 0.4;
            let now = cascade_offsets(focus, &CONFIG);
            for i in 0..CONFIG.cascades as usize {
                let delta = now[i] - prev[i];
                assert!(
                    delta.x.abs() <= 1 && delta.y.abs() <= 1 && delta.z.abs() <= 1,
                    "级 {i} 的偏移一步跳了 {delta:?}"
                );
                assert!(delta.x >= 0, "正向移动时偏移倒退: {delta:?}");
            }
            prev = now;
        }
    }

    /// 球判据与体积坐标互证：级内样本的 pv 必在 [0, res)。
    #[test]
    fn inside_samples_map_inside_volume() {
        let center = Vec3::new(4.0, 2.0, -3.0);
        let offsets = cascade_offsets(center, &CONFIG);
        let res = CONFIG.res as i32;
        for i in 0..64 {
            let angle = i as f32 * 0.7;
            let radius = CONFIG.voxel_min * (res as f32 * 0.5 - 1.0);
            let p = center + Vec3::new(radius * angle.cos(), 0.3, radius * angle.sin());
            let cascade = resolve_cascade(center, p, CONFIG.voxel_min, CONFIG.res, CONFIG.cascades);
            assert!(cascade.is_some());
            let voxel = voxel_size(CONFIG.voxel_min, cascade.unwrap());
            let pv = volume_position(p, voxel, offsets[cascade.unwrap() as usize], res);
            assert!(
                (0..res).contains(&pv.x) && (0..res).contains(&pv.y) && (0..res).contains(&pv.z)
            );
        }
    }

    /// 9.7.1 闸门 3：1/4 网格口径——16 帧内子像素走遍 4×4 整块且不重复（双射）。
    ///
    /// 这条钉的正是 gather 与播种"必须同格"的前提：若 7 与 16 不互质（如改成 8），
    /// 子像素会在两点间来回，gather 有一半的块查到本帧根本没播种的位置。
    #[test]
    fn seed_subsample_walks_whole_block_once() {
        let mut seen = std::collections::BTreeSet::new();
        for frame in 0..16u32 {
            let (sx, sy) = seed_subsample(frame);
            assert!(
                sx < 4 && sy < 4,
                "frame={frame} sub=({sx},{sy}) 越出 4×4 块"
            );
            seen.insert((sx, sy));
        }
        assert_eq!(seen.len(), 16, "16 帧应覆盖整块 16 个子像素");
        // 周期性：第 16 帧回到第 0 帧
        assert_eq!(seed_subsample(16), seed_subsample(0));
        assert_eq!(seed_subsample(17), seed_subsample(1));
    }

    /// 9.7.1 闸门 3：`low_to_full` 与播种的 `full = low*4 + sub%4 / sub/4` 逐点同式。
    ///
    /// 这里把播种侧公式独立重写一遍（不调用 `low_to_full`），两种写法给出同一坐标才算同源。
    #[test]
    fn low_to_full_matches_seeding_formula() {
        for frame in [0u32, 1, 3, 7, 15, 31, 1234] {
            let sub_index = (frame.wrapping_mul(7)) % 16;
            for low in [(0u32, 0u32), (1, 2), (479, 269), (10_000, 3)] {
                let expected = (low.0 * 4 + sub_index % 4, low.1 * 4 + sub_index / 4);
                let actual = low_to_full(low, seed_subsample(frame));
                assert_eq!(actual, expected, "frame={frame} low={low:?}");
            }
        }
    }

    /// 9.7.1：三个 gather 样本点都在同一 4×4 块内，第一个是播种点、第二个是对角。
    #[test]
    fn gather_samples_stay_inside_block() {
        for frame in 0..20u32 {
            let (sx, sy) = seed_subsample(frame);
            let points = gather_sample_offsets(frame);
            assert_eq!(points[0], (sx, sy), "第一个样本必须与播种同格");
            assert_eq!(points[1], (3 - sx, 3 - sy), "第二个样本是对角");
            assert_eq!(points[2], (1, 1), "块心固定 (1,1)");
            for (px, py) in points {
                assert!(
                    px < 4 && py < 4,
                    "frame={frame} 样本 ({px},{py}) 越出 4×4 块"
                );
            }
        }
    }
}
