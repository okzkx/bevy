// 9.7.5b 全分辨率双边上采样（compute）：把 1/4 目标的 L0+L1 按 4 邻居双边加权插值到全分辨率，
// 并用**全分辨率着色法线**求值成 E(n) 写 `scgi_irradiance`（rgb = 辐照度，a = 覆盖率）。
//
// 与 9.7.4 的分工：查表只在 1/4 做（成本 ∝ 分辨率/16），这里只付一次采样 + 一次 SH 求值。
// 权重 = 双线性 × 法线相似度（点积 8 次方）× 平面距离指数衰减 + 0.01 倍纯双线性兜底：
// 兜底项保证"四个邻居全被法线/深度拒掉"时仍有值可用，而不是黑块。
//
// `mode = 1` 走纯双线性（A/B 对照，判定线⑤读图用）。
//
// 线程 = 全分辨率像素（每线程一个）。
//
// 接口：set0 帧 UBO；set1 G-buffer（b1 normal / b3 depth）；set2 目标（b0/b1 读 1/4、
//       b4 写全分辨率）；push 24B。
// #import frame
// #import gi_compression

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var scgi_l0_in: texture_storage_2d<rgba16float, read>;
@group(2) @binding(1) var scgi_l1_in: texture_storage_2d<rgba8unorm, read>;
@group(2) @binding(4) var scgi_irradiance_out: texture_storage_2d<rgba16float, write>;

// push 布局与 `pack_upsample_push` 一一对应。
struct PushUpsample {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    // 0 = 双边；1 = 纯双线性（A/B）。
    mode: u32,
    reserved: u32,
}
var<immediate> push: PushUpsample;

const UPSAMPLE_NORMAL_POWER: i32 = 8;
const UPSAMPLE_PLANAR_FALLOFF: f32 = 0.05;
// 纯双线性兜底的比例（4 邻居全被拒时的下限）。
const BILINEAR_FALLBACK: f32 = 0.01;

fn low_texel_coord(low: vec2i) -> vec2i {
    return clamp(low, vec2i(0, 0), vec2i(i32(push.low_w) - 1, i32(push.low_h) - 1));
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    if (gid.x >= push.full_w * push.full_h) {
        return;
    }
    let pixel = vec2i(i32(gid.x % push.full_w), i32(gid.x / push.full_w));
    let coord = clamp(pixel, vec2i(0, 0), vec2i(i32(push.full_w) - 1, i32(push.full_h) - 1));
    let depth = textureLoad(depth_tex, coord, 0).r;
    if (depth <= 0.0) {
        // 背景：写 0 + a = 0（合成端不接管）
        textureStore(scgi_irradiance_out, pixel, vec4f(0.0));
        return;
    }
    let normal_raw = textureLoad(normal_tex, coord, 0).xyz;
    if (dot(normal_raw, normal_raw) < 1e-8) {
        // 法线坏了（不该发生）：只写覆盖率，让合成端按 a 决定接不接管
        textureStore(scgi_irradiance_out, pixel, vec4f(0.0, 0.0, 0.0, 0.0));
        return;
    }
    let normal = normalize(normal_raw);
    // 全分辨率像素中心 → 1/4 目标空间的连续坐标（1/4 像素 = 4 个全分辨率像素）
    let low_pos = (vec2f(f32(pixel.x), f32(pixel.y)) + vec2f(0.5)) * 0.25 - vec2f(0.5);
    let base = vec2i(floor(low_pos));
    let frac = low_pos - vec2f(base);
    var texel_of = array<vec2i, 4>(vec2i(0, 0), vec2i(1, 0), vec2i(0, 1), vec2i(1, 1));
    // 双线性权重（与 frac 的位置对应）
    var bilinear = array<f32, 4>(
        (1.0 - frac.x) * (1.0 - frac.y),
        frac.x * (1.0 - frac.y),
        (1.0 - frac.x) * frac.y,
        frac.x * frac.y,
    );

    var acc_l0 = vec3f(0.0);
    var acc_x = 0.0;
    var acc_y = 0.0;
    var acc_z = 0.0;
    var weight_sum = 0.0;
    var coverage = 0.0;
    var fallback_l0 = vec3f(0.0);
    var fallback_x = 0.0;
    var fallback_y = 0.0;
    var fallback_z = 0.0;

    for (var i = 0u; i < 4u; i = i + 1u) {
        let low = low_texel_coord(base + texel_of[i]);
        let low_pixel = low * 4 + vec2i(2, 2);
        let neighbor_depth = textureLoad(depth_tex, low_pixel, 0).r;
        let neighbor_normal = textureLoad(normal_tex, low_pixel, 0).xyz;
        let l0_texel = textureLoad(scgi_l0_in, low);
        let l1_texel = textureLoad(scgi_l1_in, low);
        let b = bilinear[i];
        fallback_l0 = fallback_l0 + l0_texel.xyz * b;
        fallback_x = fallback_x + l1_texel.x * b;
        fallback_y = fallback_y + l1_texel.y * b;
        fallback_z = fallback_z + l1_texel.z * b;
        coverage = max(coverage, l0_texel.w);

        var weight = b;
        if (push.mode == 0u) {
            if (neighbor_depth <= 0.0) {
                weight = weight * BILINEAR_FALLBACK;
            } else {
                let nd = clamp(dot(normal, normalize(neighbor_normal)), 0.0, 1.0);
                let normal_w = pow(nd, f32(UPSAMPLE_NORMAL_POWER));
                let rel = abs(neighbor_depth - depth) / max(depth, 1e-6);
                let planar_w = exp(-rel / UPSAMPLE_PLANAR_FALLOFF);
                weight = weight * (normal_w * planar_w + BILINEAR_FALLBACK);
            }
        }
        acc_l0 = acc_l0 + l0_texel.xyz * weight;
        acc_x = acc_x + l1_texel.x * weight;
        acc_y = acc_y + l1_texel.y * weight;
        acc_z = acc_z + l1_texel.z * weight;
        weight_sum = weight_sum + weight;
    }

    var l0 = fallback_l0;
    var ex = 0.5;
    var ey = 0.5;
    var ez = 0.5;
    if (weight_sum > 0.0) {
        l0 = acc_l0 / weight_sum;
        ex = acc_x / weight_sum;
        ey = acc_y / weight_sum;
        ez = acc_z / weight_sum;
    }
    let l1x = l1_decode_axis(ex, l0);
    let l1y = l1_decode_axis(ey, l0);
    let l1z = l1_decode_axis(ez, l0);
    let irradiance = scgi_evaluate(l0, l1x, l1y, l1z, normal);
    textureStore(scgi_irradiance_out, pixel, vec4f(irradiance, coverage));
}
