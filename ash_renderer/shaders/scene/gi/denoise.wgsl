// 9.7.5a 1/4 目标去噪（compute）：一遍 3×3，步长由 push 给（{1,2,2,3}[k%4]）。
//
// 权重 = 高斯 × 法线相似度（点积的 8 次方）× 平面距离衰减：只在法线相近、且到中心像素的
// 视空间平面距离小于阈值时才混，跨边不混（否则会把墙和地板糊在一起）。
// 读 A 组、写 B 组（或反过来，由调用方给的描述符集决定），因此同一个 dispatch 内不存在
// 边写边读同一张图的调度依赖。
//
// 线程 = 1/4 像素（每线程一个 1/4 目标像素）。
//
// 接口：set0 帧 UBO；set1 G-buffer（b1 normal / b3 depth，全分辨率，按 1/4 像素块心取样）；
//       set2 目标（b0/b1 读、b2/b3 写）；push 24B。
// #import frame
// #import gi_compression

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var scgi_l0_in: texture_storage_2d<rgba16float, read>;
@group(2) @binding(1) var scgi_l1_in: texture_storage_2d<rgba8unorm, read>;
@group(2) @binding(2) var scgi_l0_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(3) var scgi_l1_out: texture_storage_2d<rgba8unorm, write>;

// push 布局与 `pack_denoise_push` 一一对应。
struct PushDenoise {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    // 3×3 的采样步长（1/4 目标像素单位）。
    step: u32,
    // 保留（A/B 对照用）。
    reserved: u32,
}
var<immediate> push: PushDenoise;

// 法线相似度的指数：点积的 8 次方（Unity 同量级）。
const NORMAL_POWER: i32 = 8;
// 平面距离衰减的尺度：以中心像素的视深为单位（0.05 = 5% 深度差）。
const PLANAR_FALLOFF: f32 = 0.05;

// 邻域像素的接受权重：0 = 完全拒绝。
fn neighbor_weight(center_depth: f32, center_normal: vec3f, neighbor_depth: f32, neighbor_normal: vec3f, offset: vec2i) -> f32 {
    if (neighbor_depth <= 0.0 || center_depth <= 0.0) {
        return 0.0;
    }
    // 高斯：按 3×3 的偏移量给（sigma ≈ 1 个 1/4 像素）
    let r2 = f32(offset.x * offset.x + offset.y * offset.y);
    let gauss = exp(-r2 / 2.0);
    // 法线相似度：点积的 8 次方，跨边时接近 0
    let nd = clamp(dot(center_normal, neighbor_normal), 0.0, 1.0);
    let normal_w = pow(nd, f32(NORMAL_POWER));
    // 平面距离：相对视深差超过 PLANAR_FALLOFF 就衰减
    let rel = abs(neighbor_depth - center_depth) / max(center_depth, 1e-6);
    let planar_w = exp(-rel / PLANAR_FALLOFF);
    return gauss * normal_w * planar_w;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    if (gid.x >= push.low_w * push.low_h) {
        return;
    }
    let low = vec2i(i32(gid.x % push.low_w), i32(gid.x / push.low_w));
    let center_pixel = low * 4 + vec2i(2, 2);
    let center_depth = textureLoad(depth_tex, center_pixel, 0).r;
    let center_normal = textureLoad(normal_tex, center_pixel, 0).xyz;

    var acc_l0 = vec3f(0.0);
    var acc_ratio = vec3f(0.0);
    var acc_alpha = 0.0;
    var weight_sum = 0.0;
    let step = i32(max(push.step, 1u));
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let offset = vec2i(dx * step, dy * step);
            let coord = low + offset;
            if (coord.x < 0 || coord.y < 0 || coord.x >= i32(push.low_w) || coord.y >= i32(push.low_h)) {
                continue;
            }
            let pixel = coord * 4 + vec2i(2, 2);
            let depth = textureLoad(depth_tex, pixel, 0).r;
            let normal = textureLoad(normal_tex, pixel, 0).xyz;
            let weight = neighbor_weight(center_depth, center_normal, depth, normal, offset);
            if (weight <= 0.0) {
                continue;
            }
            let sample_l0 = textureLoad(scgi_l0_in, coord);
            let sample_l1 = textureLoad(scgi_l1_in, coord);
            acc_l0 = acc_l0 + sample_l0.xyz * weight;
            acc_ratio = acc_ratio + sample_l1.xyz * weight;
            acc_alpha = acc_alpha + sample_l0.w * weight;
            weight_sum = weight_sum + weight;
        }
    }

    var out_l0 = vec3f(0.0);
    var out_ratio = vec3f(0.5);
    var out_alpha = 0.0;
    if (weight_sum > 0.0) {
        out_l0 = acc_l0 / weight_sum;
        out_ratio = acc_ratio / weight_sum;
        out_alpha = acc_alpha / weight_sum;
    }
    textureStore(scgi_l0_out, low, vec4f(out_l0, out_alpha));
    textureStore(scgi_l1_out, low, vec4f(out_ratio, 0.5));
}
