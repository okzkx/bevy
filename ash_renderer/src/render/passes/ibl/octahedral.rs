//! 八面体球↔方映射与 16 方向桶索引的 CPU 镜像。GPU 正本在
//! `shaders/common/octahedral.wgsl`（GlobalProbe 投影与着色求值共用）。
//!
//! 两份实现必须逐式一致（同一 `res`、同一"×res 截断再钳 res−1"的取整语义）：
//! shader 端供投影与着色，宿主端供 `ibl_stats` 的六方向摘要与单元测试。
//! 改一处必须同步另一处。

/// `oct_wrap`：下半球折叠用的副对角线翻转（Unity `OctWrap` 同式）。
fn oct_wrap(v: [f32; 2]) -> [f32; 2] {
    [
        (1.0 - v[1].abs()) * if v[0] >= 0.0 { 1.0 } else { -1.0 },
        (1.0 - v[0].abs()) * if v[1] >= 0.0 { 1.0 } else { -1.0 },
    ]
}

/// 方向（单位向量）→ 八面体方图坐标 `[0,1]²`；零向量按极小分母兜底（不静默 NaN）。
pub(crate) fn sphere_to_square(n: [f32; 3]) -> [f32; 2] {
    let denominator = (n[0].abs() + n[1].abs() + n[2].abs()).max(1e-20);
    let p = [n[0] / denominator, n[1] / denominator, n[2] / denominator];
    let xy = if p[2] < 0.0 {
        oct_wrap([p[0], p[1]])
    } else {
        [p[0], p[1]]
    };
    [xy[0] * 0.5 + 0.5, xy[1] * 0.5 + 0.5]
}

/// 八面体方图坐标 `[0,1]²` → 单位方向。
pub(crate) fn square_to_sphere(f: [f32; 2]) -> [f32; 3] {
    let g = [f[0] * 2.0 - 1.0, f[1] * 2.0 - 1.0];
    let mut n = [g[0], g[1], 1.0 - g[0].abs() - g[1].abs()];
    let t = (-n[2]).clamp(0.0, 1.0);
    n[0] += if n[0] >= 0.0 { -t } else { t };
    n[1] += if n[1] >= 0.0 { -t } else { t };
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-20);
    [n[0] / length, n[1] / length, n[2] / length]
}

/// 方向 → 桶索引（行主序 `y * res + x`）。与 Unity 同式：方图坐标先乘 `res`、截断，
/// 再钳到 `res−1`——乘 `res−1` 会把桶边界压歪，方向与桶不再互逆。
pub(crate) fn direction_to_bucket(n: [f32; 3], res: u32) -> u32 {
    let s = sphere_to_square(n);
    let scale = res as f32;
    let x = ((s[0] * scale) as u32).min(res - 1);
    let y = ((s[1] * scale) as u32).min(res - 1);
    y * res + x
}

/// 桶索引 → 桶中心方向（`(x+0.5, y+0.5) / res` 反投影）。与 shader 的
/// `bucket_index_to_direction` 同式；`ibl_census` 的桶取证回显桶中心。
pub(crate) fn bucket_to_direction(index: u32, res: u32) -> [f32; 3] {
    let x = (index % res) as f32;
    let y = (index / res) as f32;
    let scale = res as f32;
    square_to_sphere([(x + 0.5) / scale, (y + 0.5) / scale])
}

/// 一个桶的 RGBL1 求值：`E(n) = l0 + l1x·n.x + l1y·n.y + l1z·n.z`。存储系数已折
/// `Y00`/`Y1` 基常数，求值不再乘（GPU 端 `probe16_eval` 同式）。
pub(crate) fn eval_probe_bucket(n: [f32; 3], bucket: &[[f32; 4]; 4]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for (channel, value) in out.iter_mut().enumerate() {
        *value = bucket[0][channel]
            + bucket[1][channel] * n[0]
            + bucket[2][channel] * n[1]
            + bucket[3][channel] * n[2];
    }
    out
}

#[cfg(test)]
mod tests {
    use bevy::math::ops::{acos, cos, sin};

    use super::super::environment::{IBL_PROBE_BUCKETS, IBL_PROBE_RESOLUTION};
    use super::*;

    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    /// 确定性方向表：黄金角螺旋，避开随机源依赖。
    fn sample_directions(count: usize) -> Vec<[f32; 3]> {
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        (0..count)
            .map(|i| {
                let z = 1.0 - 2.0 * (i as f32 + 0.5) / count as f32;
                let r = (1.0 - z * z).max(0.0).sqrt();
                let phi = golden * i as f32;
                [r * cos(phi), r * sin(phi), z]
            })
            .collect()
    }

    /// 六轴落桶与 Unity 同式（2026-11-23 对照 `GlobalProbe.hlsl` 手算）。
    #[test]
    fn six_axes_hit_expected_buckets() {
        let res = IBL_PROBE_RESOLUTION;
        assert_eq!(direction_to_bucket([1.0, 0.0, 0.0], res), 11);
        assert_eq!(direction_to_bucket([-1.0, 0.0, 0.0], res), 8);
        assert_eq!(direction_to_bucket([0.0, 1.0, 0.0], res), 14);
        assert_eq!(direction_to_bucket([0.0, -1.0, 0.0], res), 2);
        assert_eq!(direction_to_bucket([0.0, 0.0, 1.0], res), 10);
        assert_eq!(direction_to_bucket([0.0, 0.0, -1.0], res), 15);
    }

    /// 球→方→球往返：方向一致（含下半球四个折角）。
    #[test]
    fn sphere_square_sphere_round_trips() {
        for n in sample_directions(512) {
            let back = square_to_sphere(sphere_to_square(n));
            assert!(
                dot(n, back) > 0.99999,
                "往返失真: {n:?} -> {back:?}（dot = {}）",
                dot(n, back)
            );
        }
    }

    /// 方向→桶→桶中心：夹角限制在半个八面体方格对角线内（实测最坏约 48°，
    /// 方格在八面体空间而非球面 Voronoi，松到 55° 只抓真错位）。
    #[test]
    fn bucket_round_trip_stays_inside_cell() {
        let res = IBL_PROBE_RESOLUTION;
        for n in sample_directions(512) {
            let index = direction_to_bucket(n, res);
            assert!(index < IBL_PROBE_BUCKETS);
            let center = bucket_to_direction(index, res);
            let angle = acos(dot(n, center).clamp(-1.0, 1.0)).to_degrees();
            assert!(angle <= 55.0, "桶 {index} 中心偏 {angle}°（n = {n:?}）");
        }
    }

    /// 求值式与"逐通道点积"等价：l0 + l1·n。
    #[test]
    fn eval_matches_plain_l1() {
        let bucket = [
            [1.0, 2.0, 3.0, 0.0],
            [0.5, -0.25, 0.0, 0.0],
            [0.0, 1.0, -1.0, 0.0],
            [-2.0, 0.0, 0.5, 0.0],
        ];
        let n = [0.3, -0.4, 0.866];
        let e = eval_probe_bucket(n, &bucket);
        for channel in 0..3 {
            let expected = bucket[0][channel]
                + bucket[1][channel] * n[0]
                + bucket[2][channel] * n[1]
                + bucket[3][channel] * n[2];
            assert!((e[channel] - expected).abs() < 1e-6);
        }
    }
}
