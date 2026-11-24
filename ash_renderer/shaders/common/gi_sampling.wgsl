// 9.6 采样基座与 RGBL1 折叠（公共片段）：逐 patch 低差异序列（Kronecker 2D）、均匀半球
// 采样、low-bias hash、球谐投影与余弦卷积。只放纯函数与常量，不声明任何 @group/@binding；
// 入口文件自行声明它读的全局（本片段不读全局）。
//
// 出处（2026-11-24 对照本机 Unity 7000.0.0a9 包内源码，路径见八段拆解 §8.1）：
// - `Runtime/Sampling/QuasiRandom.hlsl` 的 `QrngKronecker2D`（R2 序列 + Cranley-Patterson 旋转，
//   下标周期 2^20）
// - `Runtime/Sampling/Hashes.hlsl` 的 `LowBiasHash32`
// - `Runtime/Sampling/Common.hlsl` 的 `UintToFloat01` / `UniformHemisphereSample` /
//   `OrthoBasisFromVector` / `CosineSample` / `PowerHeuristic`
// - `Runtime/Lighting/SurfaceCache/Estimation.compute` 的 `ProjectAndAccumulate` 与
//   `Lighting/SurfaceCache/Common.hlsl` 命名空间 `SphericalHarmonics` 的 `CosineConvolve`
//
// 与本工程的接缝（见施工记录 9.6.1）：
// - **投影出的系数已含 Y 基常数**（`y0` / `y1`）：`Eval(n) = l0 + l1x·n.x + l1y·n.y + l1z·n.z`
//   即求值式，与 9.3 的 GlobalProbe 和 `patch_alloc.wgsl` 的 IBL 种子同口径。
// - 余弦卷积的 `A0 = π`、`A1 = 2π/3` 与基常数合并成两个乘数（`GI_FOLD_L0` / `GI_FOLD_L1`）：
//   卷积后写进 `PatchIrradiance` 的那份就是"已折余弦卷积 + 基常数"的求值式，消费端只再乘
//   `albedo/π`（`gbuffer_composite.wgsl` 的 `Fd_Lambert`），**不要**在这里再乘一次 π。
// - 半球采样是**均匀**的（`u.x` 直接当 z，不做 sqrt 参数化），归一化常数 `1/(2π)`；
//   余弦加权版本 `cosine_hemisphere_sample` 只作 A/B 对照，不默认开。

// Y 基常数（`kSHBasis0` / `kSHBasis1`）。
const GI_SH_Y0: f32 = 0.2820947917738781;
const GI_SH_Y1: f32 = 0.4886025119029199;
// 余弦卷积 A0 = π、A1 = 2π/3 与上面两个基常数合并后的乘数：
//   GI_FOLD_L0 = π · Y00 = √π/2          = 0.88622692545275801365
//   GI_FOLD_L1 = (2π/3) · Y1 = (2π/3)·Y1 = 1.02332670794648857622
const GI_FOLD_L0: f32 = 0.8862269254527581;
const GI_FOLD_L1: f32 = 1.0233267079464885;
// 均匀半球密度 1/(2π)：`ProjectAndAccumulate` 的 1/pdf 乘数就是它的倒数（= 2π）。
const GI_UNIFORM_HEMISPHERE_DENSITY: f32 = 0.15915494309189535;
const GI_TWO_PI: f32 = 6.283185307179586;
// Kronecker 序列下标周期（Unity 同值）：超过它数值精度会掉，届时换序列。
const GI_R2_PERIOD: u32 = 1048576u;
// 浮点 1 − ε（Unity 的 `FLOAT_ONE_MINUS_EPSILON`，1 − 2⁻²⁴）。
const GI_ONE_MINUS_EPSILON: f32 = 0.99999994;

// 32 位 low-bias hash（Jarzynski–Olano，`Hashes.hlsl` 的 `LowBiasHash32`）。
fn low_bias_hash_32(value: u32) -> u32 {
    var x = value;
    x = x ^ (x >> 16u);
    x = x * 0x21F0AADu;
    x = x ^ (x >> 15u);
    x = x * 0xD35A2D97u;
    x = x ^ (x >> 15u);
    return x;
}

// 加种子的版本：先加种子再做同一串（Unity 的默认参数 `seed = 0` 走的是这条）。
fn low_bias_hash_32_seeded(value: u32, seed: u32) -> u32 {
    return low_bias_hash_32(value + seed);
}

// u32 → [0, 1) 浮点（`UintToFloat01`）：乘 2⁻³² 并夹到 1 − ε。
fn gi_uint_to_float01(x: u32) -> f32 {
    return min(f32(x) * 2.3283064365386963e-10, GI_ONE_MINUS_EPSILON);
}

// Kronecker 2D 采样器状态（`QrngKronecker2D` 的两个字段）。
struct GiR2 {
    // Cranley-Patterson 旋转种子（= patch 哈希）。
    cp_seed: u32,
    // 已按种子平移过的样本下标（模 2^20）。
    index: u32,
}

// 初始化：`startIndex` 是本次估计的样本起点（预热期与稳态期不同，见 estimation.wgsl）。
fn gi_r2_init(seed: u32, start_index: u32) -> GiR2 {
    var s: GiR2;
    s.cp_seed = seed;
    s.index = (start_index + seed) % GI_R2_PERIOD;
    return s;
}

// alpha 表（R2 + K21_2，值已乘 2^32）用 switch 取，避免动态下标索引常量数组。
fn gi_r2_alpha(dimension: u32) -> u32 {
    switch dimension % 4u {
        case 0u: {
            return 3242174889u;
        }
        case 1u: {
            return 2447445414u;
        }
        case 2u: {
            return 3316612456u;
        }
        default: {
            return 1538627358u;
        }
    }
}

// 取一维样本：Kronecker 序列 `cpRot + alpha·index`（u32 回绕即模 1）。
fn gi_r2_float(s: GiR2, dimension: u32) -> f32 {
    let rot = s.cp_seed + 3646589397u * (dimension / 4u);
    return gi_uint_to_float01(rot + gi_r2_alpha(dimension) * s.index);
}

// 取二维样本：dimension 与 dimension+1 两维（Unity 的 `GetSample`）。
fn gi_r2_sample2(s: GiR2, dimension: u32) -> vec2f {
    return vec2f(gi_r2_float(s, dimension * 2u), gi_r2_float(s, dimension * 2u + 1u));
}

// 步进一个样本（模 2^20）。
fn gi_r2_next(s: ptr<function, GiR2>) {
    let next_index = (*s).index + 1u;
    (*s).index = select(next_index, 0u, next_index >= GI_R2_PERIOD);
}

// 由法线构造正交基（`OrthoBasisFromVector`）：列向量 = (t, b, n)，`basis * local` 即世界方向。
fn gi_tangent_basis(n: vec3f) -> mat3x3f {
    let sign = select(-1.0, 1.0, n.z >= 0.0);
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    let t = vec3f(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x);
    let bt = vec3f(b, sign + n.y * n.y * a, -n.y);
    return mat3x3f(t, bt, n);
}

// 均匀半球采样（`UniformHemisphereSample`）：z 均匀、方位角均匀，归一化常数 1/(2π)。
fn uniform_hemisphere_sample(u: vec2f, n: vec3f) -> vec3f {
    let z = u.x;
    let r = sqrt(max(0.0, 1.0 - z * z));
    let phi = GI_TWO_PI * u.y;
    let local = vec3f(r * cos(phi), r * sin(phi), z);
    return gi_tangent_basis(n) * local;
}

// 余弦加权半球采样（`CosineSample`）：仅 A/B 对照用，默认关（权重推导与 MIS 概率不同）。
fn cosine_hemisphere_sample(u: vec2f, n: vec3f) -> vec3f {
    let a = sqrt(u.x);
    let phi = GI_TWO_PI * u.y;
    let local = vec3f(a * cos(phi), a * sin(phi), sqrt(max(0.0, 1.0 - u.x)));
    return gi_tangent_basis(n) * local;
}

// 平衡启发式（`PowerHeuristic`）：MIS 的两路概率权重。
fn power_heuristic(f: f32, b: f32) -> f32 {
    let q = f * f + b * b;
    return select((f * f) / q, 0.0, q <= 0.0);
}

// RGBL1 累加器：求值式（已含 Y 基常数），与 `PatchIrradiance` 同序。
struct GiRgbl1 {
    l0: vec3f,
    l1x: vec3f,
    l1y: vec3f,
    l1z: vec3f,
}

fn gi_rgbl1_zero() -> GiRgbl1 {
    var out: GiRgbl1;
    out.l0 = vec3f(0.0);
    out.l1x = vec3f(0.0);
    out.l1y = vec3f(0.0);
    out.l1z = vec3f(0.0);
    return out;
}

// 把一条样本的辐射亮度按方向累加进 SH（`ProjectAndAccumulate`）。
// 归一化（1/pdf 与 1/N）由调用方在累加前乘进 radiance —— Unity 是把 N 段各自归一化后相加。
fn gi_project_accumulate(acc: ptr<function, GiRgbl1>, radiance: vec3f, direction: vec3f) {
    (*acc).l0 += radiance * GI_SH_Y0;
    (*acc).l1x += radiance * GI_SH_Y1 * direction.x;
    (*acc).l1y += radiance * GI_SH_Y1 * direction.y;
    (*acc).l1z += radiance * GI_SH_Y1 * direction.z;
}

// acc += other * scale（三路直接光各自的归一化不同，统一在这里合流）。
fn gi_rgbl1_add_scaled(acc: ptr<function, GiRgbl1>, other: GiRgbl1, scale: f32) {
    (*acc).l0 += other.l0 * scale;
    (*acc).l1x += other.l1x * scale;
    (*acc).l1y += other.l1y * scale;
    (*acc).l1z += other.l1z * scale;
}

fn gi_rgbl1_scale(v: GiRgbl1, scale: f32) -> GiRgbl1 {
    var out: GiRgbl1;
    out.l0 = v.l0 * scale;
    out.l1x = v.l1x * scale;
    out.l1y = v.l1y * scale;
    out.l1z = v.l1z * scale;
    return out;
}

// 余弦卷积（`CosineConvolve` 的 A0/A1）与 Y 基常数合并成一次乘法：
// 输入是"已含基常数的辐射亮度 SH"，输出是"已折余弦卷积的辐照度 SH"的求值式。
// 常量天空自检：均匀半球下 `l0 + l1x·n.x + …` 在法线方向应等于 πL。
fn gi_cosine_convolve_fold(v: GiRgbl1) -> GiRgbl1 {
    var out: GiRgbl1;
    out.l0 = v.l0 * GI_FOLD_L0;
    out.l1x = v.l1x * GI_FOLD_L1;
    out.l1y = v.l1y * GI_FOLD_L1;
    out.l1z = v.l1z * GI_FOLD_L1;
    return out;
}

// 逐分量线性插值（Unity `SphericalHarmonics::Lerp` 的 RGB 版）。
fn gi_rgbl1_lerp(a: GiRgbl1, b: GiRgbl1, s: vec3f) -> GiRgbl1 {
    var out: GiRgbl1;
    out.l0 = mix(a.l0, b.l0, s);
    out.l1x = mix(a.l1x, b.l1x, s);
    out.l1y = mix(a.l1y, b.l1y, s);
    out.l1z = mix(a.l1z, b.l1z, s);
    return out;
}
