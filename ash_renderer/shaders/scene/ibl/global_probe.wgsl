// GlobalProbe 16 桶投影（compute 单入口）：每个桶从桶中心方向的半球均匀采 32 个
// 方向，累积辐射度的 L1 投影，乘反权重 + 余弦卷积 + 基函数常数，写 16×4 vec4
//（每条 RGBL1：l0 / l1x / l1y / l1z，w 未用）。
//
// 与 Unity `GlobalProbe.compute` 同算法（SAMPLE_COUNT 32、2π/32 反权重、A0=π、
// A1=2π/3）；线程形状简化为"一桶一线程、桶内 32 样本串行"（512 次 cubemap 采样，
// 成本可忽略，省掉 workgroup 归约）。
//
// 存储约定（9.3 施工计划 §0.4）：基函数常数折进系数，求值式只剩
// `E(n) = l0 + l1x·n.x + l1y·n.y + l1z·n.z`。推导：
//   c00 = (2π/N)ΣL·Y00，存 l0 = A0·c00·Y00 = raw0·A0·Y00²；
//   c1  = (2π/N)ΣL·ω·Y1，存 l1 = A1·c1·Y1 = raw1·A1·Y1²。
// 常量天空下，l0 项给 πL/2，L1 项因半球均值方向 = 桶中心/2 再补 πL/2，合成 πL。
//
// 接口：b0 = mip0 cube 采样（9.2 捕获的锐利源，无太阳圆盘）、b1 = 采样器、
// b2 = 16 桶 buffer。dispatch = 1（16 个线程各认领一个桶）。
// #import ibl
// #import octahedral

const BUCKET_RESOLUTION: u32 = 4u;
const BUCKET_COUNT: u32 = BUCKET_RESOLUTION * BUCKET_RESOLUTION;
const SAMPLE_COUNT: u32 = 32u;
const Y00: f32 = 0.28209479;
const Y1: f32 = 0.48860251;

@group(0) @binding(0) var env_src: texture_cube<f32>;
@group(0) @binding(1) var env_samp: sampler;
@group(0) @binding(2) var<storage, read_write> probe: array<vec4f>;

// 以 n 为极轴的半球均匀采样（对立体角密度 1/(2π)）：cosθ 均匀、φ 均匀。
fn uniform_hemisphere(xi: vec2f, n: vec3f) -> vec3f {
    let cos_theta = xi.x;
    let sin_theta = sqrt(max(1.0 - cos_theta * cos_theta, 0.0));
    let phi = 2.0 * PI * xi.y;
    let local = vec3f(sin_theta * cos(phi), sin_theta * sin(phi), cos_theta);
    return normalize(tangent_frame(n) * local);
}

@compute @workgroup_size(16)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bucket = gid.x;
    if (bucket >= BUCKET_COUNT) {
        return;
    }
    let center = bucket_index_to_direction(bucket, BUCKET_RESOLUTION);
    var l0 = vec3f(0.0);
    var l1x = vec3f(0.0);
    var l1y = vec3f(0.0);
    var l1z = vec3f(0.0);
    for (var i = 0u; i < SAMPLE_COUNT; i = i + 1u) {
        // 分层中点：ξ.x = (i+0.5)/N（均匀 cosθ 的均值恰为 1/2，能量自检偏差更小；
        // 直接用 i/N 会让均值偏到 0.484，常量天空下 E 低约 1.6%）。
        let xi = vec2f((f32(i) + 0.5) / f32(SAMPLE_COUNT), radical_inverse_vdc(i));
        let dir = uniform_hemisphere(xi, center);
        let radiance = textureSampleLevel(env_src, env_samp, dir, 0.0).rgb;
        l0 += radiance;
        l1x += radiance * dir.x;
        l1y += radiance * dir.y;
        l1z += radiance * dir.z;
    }
    // 反权重 × 余弦卷积 × 基函数常数（投影端一次 Y、求值端一次 Y 都已折进来）
    let integration = 2.0 * PI / f32(SAMPLE_COUNT);
    let weight0 = integration * PI * Y00 * Y00;
    let weight1 = integration * (2.0 * PI / 3.0) * Y1 * Y1;
    let base = bucket * 4u;
    probe[base + 0u] = vec4f(l0 * weight0, 0.0);
    probe[base + 1u] = vec4f(l1x * weight1, 0.0);
    probe[base + 2u] = vec4f(l1y * weight1, 0.0);
    probe[base + 3u] = vec4f(l1z * weight1, 0.0);
}
