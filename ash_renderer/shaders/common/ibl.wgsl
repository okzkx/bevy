// 环境光照（IBL）公共片段：立方体面 ↔ 方向映射、SH-L2 辐照度求值、GGX 重要性采样
// 与 split-sum 高光求值。不声明绑定：纹理/采样器/系数数组都由调用方作为参数传入，
// 导入方自己声明 `@group/@binding` 全局量。
//
// 口径（见 `.agents/docs/开发步骤/9-表面缓存全局光照/9.2-天空盒IBL/施工计划：天空盒IBL.md`）：
// - 立方体面序按 Vulkan 约定：0=+X 1=-X 2=+Y 3=-Y 4=+Z 5=-Z；面内 u 向右、v 向下。
//   捕获（写面）与硬件 `textureSampleLevel(texture_cube)`（读面）必须同一约定，接缝
//   对不上就是这里写反了。
// - SH-L2 是 real SH（基函数表见 `sh_basis`），投影时把余弦卷积的 A_l 折进系数
//   （A0=π、A1=2π/3、A2=π/4），求值时不再乘——`sh_irradiance` 返回的就是辐照度 E(n)。
// - 高光走 split-sum：预滤波 cubemap（mip = perceptualRoughness × (mip 数−1)）× DFG LUT。
//   粗糙度必须用 **perceptualRoughness**（α = p² 直接喂 mip 会把粗糙面算得过锐）。
// #import brdf

// 预滤波 mip 上界 = 级数 − 1（与 `environment.rs` 的 IBL_MAX_MIP 同源；改级数两边一起改）
const IBL_MAX_MIP: f32 = 8.0;

// ---- 立方体面 ↔ 方向 ----

// 面内 uv ∈ [0,1]² → 世界方向（单位向量）。逆表见 Vulkan 规范的 cube map 坐标一节：
// 面 +Y 的 (s,t) 映射到 (+x, +z)，面 ±X/±Z 的 t 轴都对齐 −y。
fn cube_face_direction(face: u32, uv: vec2f) -> vec3f {
    let sc = uv.x * 2.0 - 1.0;
    let tc = uv.y * 2.0 - 1.0;
    switch face {
        case 0u: { return normalize(vec3f(1.0, -tc, -sc)); }
        case 1u: { return normalize(vec3f(-1.0, -tc, sc)); }
        case 2u: { return normalize(vec3f(sc, 1.0, tc)); }
        case 3u: { return normalize(vec3f(sc, -1.0, -tc)); }
        case 4u: { return normalize(vec3f(sc, -tc, 1.0)); }
        default: { return normalize(vec3f(-sc, -tc, -1.0)); }
    }
}

// ---- SH-L2 ----

// real SH 的 9 个基函数（参数是单位方向；极轴取 z，与投影端同一张表）。
fn sh_basis(n: vec3f) -> array<f32, 9> {
    return array<f32, 9>(
        0.28209479,
        0.48860251 * n.y,
        0.48860251 * n.z,
        0.48860251 * n.x,
        1.09254843 * n.x * n.y,
        1.09254843 * n.y * n.z,
        0.31539157 * (3.0 * n.z * n.z - 1.0),
        1.09254843 * n.x * n.z,
        0.54627422 * (n.x * n.x - n.y * n.y),
    );
}

// l 带的余弦卷积系数 A_l（折叠进存储系数，求值不再乘）。
fn sh_band_factor(index: u32) -> f32 {
    if index == 0u {
        return PI;
    }
    if index < 4u {
        return 2.0 * PI / 3.0;
    }
    return PI / 4.0;
}

// 辐照度 E(n) = Σ C_lm·Y_lm(n)：`coeffs` 是已折叠 A_l 的 9 行（xyz = RGB，w 未用）。
fn sh_irradiance(n: vec3f, coeffs: array<vec4f, 9>) -> vec3f {
    let basis = sh_basis(n);
    var e = vec3f(0.0);
    for (var i = 0u; i < 9u; i = i + 1u) {
        e = e + coeffs[i].rgb * basis[i];
    }
    return e;
}

// ---- 采样与预滤波 ----

// 以 n 为 z 轴的正交基（列 = 切线、副切线、法线）。
fn tangent_frame(n: vec3f) -> mat3x3f {
    // 挑选与 n 不平行的辅助轴，避免 cross 退化（n 接近 ±X 时换用 +Y）
    let helper = select(vec3f(1.0, 0.0, 0.0), vec3f(0.0, 1.0, 0.0), abs(n.x) > 0.99);
    let t = normalize(cross(helper, n));
    let b = cross(n, t);
    return mat3x3f(t, b, n);
}

// GGX（Trowbridge-Reitz）半程向量重要性采样：xi 两个均匀随机数，a = α = p²。
// 返回世界空间的 H（以 n 为极轴）。
fn importance_sample_ggx(xi: vec2f, n: vec3f, a: f32) -> vec3f {
    let phi = 2.0 * PI * xi.x;
    let cos_theta = sqrt((1.0 - xi.y) / max(1.0 + (a * a - 1.0) * xi.y, 1e-6));
    let sin_theta = sqrt(max(1.0 - cos_theta * cos_theta, 0.0));
    let h = vec3f(cos(phi) * sin_theta, sin(phi) * sin_theta, cos_theta);
    return normalize(tangent_frame(n) * h);
}

// 低差异序列（Hammersley）：x = i/N，y = 二进制反转。预滤波、DFG LUT 与 SH 投影
// 用同一实现，保证同一批方向在两处一致。
fn radical_inverse_vdc(bits_in: u32) -> f32 {
    var bits = bits_in;
    bits = (bits << 16u) | (bits >> 16u);
    bits = ((bits & 0x55555555u) << 1u) | ((bits & 0xAAAAAAAAu) >> 1u);
    bits = ((bits & 0x33333333u) << 2u) | ((bits & 0xCCCCCCCCu) >> 2u);
    bits = ((bits & 0x0F0F0F0Fu) << 4u) | ((bits & 0xF0F0F0F0u) >> 4u);
    bits = ((bits & 0x00FF00FFu) << 8u) | ((bits & 0xFF00FF00u) >> 8u);
    return f32(bits) * 2.3283064365386963e-10;
}

fn hammersley(i: u32, n: u32) -> vec2f {
    return vec2f(f32(i) / f32(n), radical_inverse_vdc(i));
}

// 球面均匀采样：xi.x 决定 cosθ（z 均匀）、xi.y 决定方位角。SH 投影的自检参照
// （E_ref 的暴力余弦积分）与投影样本共用这条映射。
fn uniform_sphere(xi: vec2f) -> vec3f {
    let z = 1.0 - 2.0 * xi.x;
    let r = sqrt(max(1.0 - z * z, 0.0));
    let phi = 2.0 * PI * xi.y;
    return vec3f(r * cos(phi), r * sin(phi), z);
}

// ---- split-sum 高光 ----

// 预滤波 cubemap 采样：mip = perceptualRoughness × (mip 数−1)，硬件三线性在相邻
// 预滤波级之间插值。
fn ibl_prefiltered(
    env: texture_cube<f32>,
    samp: sampler,
    r: vec3f,
    perceptual_roughness: f32,
    max_mip: f32,
) -> vec3f {
    return textureSampleLevel(env, samp, r, perceptual_roughness * max_mip).rgb;
}

// DFG（split-sum 第二项）：x = A、y = B，坐标为 (NoV, perceptualRoughness)。
fn ibl_dfg(dfg: texture_2d<f32>, samp: sampler, no_v: f32, perceptual_roughness: f32) -> vec2f {
    return textureSampleLevel(dfg, samp, vec2f(no_v, perceptual_roughness), 0.0).rg;
}

// split_sum 综合：prefiltered(R) · (F0·A + B)。R = reflect(−V, n)。
fn ibl_specular(
    env: texture_cube<f32>,
    samp: sampler,
    dfg: texture_2d<f32>,
    n: vec3f,
    v: vec3f,
    no_v: f32,
    f0: vec3f,
    perceptual_roughness: f32,
    max_mip: f32,
) -> vec3f {
    let r = reflect(-v, n);
    let pre = ibl_prefiltered(env, samp, r, perceptual_roughness, max_mip);
    let ab = ibl_dfg(dfg, samp, no_v, perceptual_roughness);
    return pre * (f0 * ab.x + vec3f(ab.y));
}
