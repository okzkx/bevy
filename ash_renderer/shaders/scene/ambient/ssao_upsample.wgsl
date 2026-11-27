// 半分辨率 AO → 全分辨率 AO（10.2.3）：2×2 象限双边上采样。
//
// 每个全分辨率像素取它落在的半分辨率 2×2 邻域，权重 = 双线性权重 × 深度相似权重。
// 深度相似按视深差（米）算——跨到前/后景的邻居被压下去，几何边缘不会被抹平成 2×2
// 块，地平线也不会 halo（R4）。5×5 邻域会吃掉 0.3 ms 预算的大半，这里只算 4 个纹素。
//
// 视深用投影矩阵第 4 行点积直接算（clip.w），不做「反投影回世界再相减」：后者每像素
// 要 5 次 mat4 乘，实测是 release 下本 pass 的主要开销（判定线 6a 一度 0.37 ms）。
//
// 曾试过「每个半分辨率纹素线程处理 2×2 全分辨率像素、3×3 邻域共享读取」：寄存器数组
// 会被压到 local memory，实测反而 0.375 ms，已回退到逐像素版本。
// #import frame

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var ao_half_tex: texture_storage_2d<r32float, read>;
@group(2) @binding(1) var ao_full: texture_storage_2d<r32float, write>;

struct PushUpsample {
    full_w: u32,
    full_h: u32,
    sigma: f32,
    pad: u32,
}

var<immediate> push: PushUpsample;

// 视深：投影矩阵第 4 行点积（透视下即沿视线方向的正距离）。
fn clip_w(clip: vec4f) -> f32 {
    let row = vec4f(
        frame.view_proj[0].w,
        frame.view_proj[1].w,
        frame.view_proj[2].w,
        frame.view_proj[3].w,
    );
    return dot(row, clip);
}

fn ndc_of(pixel: vec2f, depth: f32) -> vec4f {
    return vec4f(
        pixel.x / f32(push.full_w) * 2.0 - 1.0,
        1.0 - pixel.y / f32(push.full_h) * 2.0,
        depth,
        1.0,
    );
}

@compute @workgroup_size(8, 8)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let full = vec2u(push.full_w, push.full_h);
    let half = vec2u((push.full_w + 1u) / 2u, (push.full_h + 1u) / 2u);
    if (gid.x >= full.x || gid.y >= full.y) {
        return;
    }
    let half_max = vec2i(half) - vec2i(1);
    let p = vec2i(gid.xy);
    let center_uv = (vec2f(p) + vec2f(0.5)) / vec2f(full);
    let center_depth = textureLoad(depth_tex, p, 0).r;
    if (center_depth <= 1e-6) {
        textureStore(ao_full, p, vec4f(1.0, 0.0, 0.0, 0.0));
        return;
    }
    let center_vd = clip_w(ndc_of(vec2f(p) + vec2f(0.5), center_depth));
    // 该像素中心落在半分辨率网格的哪个 2×2 象限（低分辨率像素中心 + 小数部分）。
    let low_center = (vec2f(p) + vec2f(0.5)) * 0.5 - vec2f(0.5);
    let base = vec2i(floor(low_center));
    let frac = low_center - floor(low_center);
    let sigma2 = 2.0 * push.sigma * push.sigma;

    var sum = 0.0;
    var weight_sum = 0.0;
    for (var j: i32 = 0; j < 2; j = j + 1) {
        for (var i: i32 = 0; i < 2; i = i + 1) {
            let coord = clamp(base + vec2i(j, i), vec2i(0), half_max);
            let bilinear = select(1.0 - frac.x, frac.x, j == 1)
                * select(1.0 - frac.y, frac.y, i == 1);
            var w = bilinear;
            let sd = textureLoad(depth_tex, coord * 2, 0).r;
            if (sd > 1e-6) {
                let sndc = ndc_of(vec2f(coord * 2) + vec2f(0.5), sd);
                let delta = clip_w(sndc) - center_vd;
                w = w * exp(-delta * delta / sigma2);
            }
            sum = sum + textureLoad(ao_half_tex, coord).r * w;
            weight_sum = weight_sum + w;
        }
    }

    var ao = 1.0;
    if (weight_sum > 1e-6) {
        ao = sum / weight_sum;
    } else {
        // 4 个邻居都跨到别的表面（细柱/边缘）：退回最近的半分辨率纹素。
        let nearest = clamp(vec2i(low_center + vec2f(0.5)), vec2i(0), half_max);
        ao = textureLoad(ao_half_tex, nearest).r;
    }
    textureStore(ao_full, p, vec4f(ao, 0.0, 0.0, 0.0));
}
