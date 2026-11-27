// 半分辨率 AO → 全分辨率 AO（10.2.3）：2×2 象限双边上采样。
//
// 每个全分辨率像素取它落在的半分辨率 2×2 邻域，权重 = 双线性权重 × 深度相似权重；
// 深度相似按世界位置差（米）算——跨到前/后景的邻居被压下去，几何边缘不会被抹平成
// 2×2 块，地平线也不会 halo（R4）。只算 4 个纹素：5×5 会吃掉 0.3 ms 预算的大半。
//
// 半分辨率 AO 与全分辨率 AO 都在 GENERAL 布局、都由本 pass 的 storage 集绑着；
// 天空像素直接写 1.0（关档时合成乘 1.0 也一样）。
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

// 深度反投影：uv → shader-NDC → clip → world（与 gbuffer_composite 同式）。
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

@compute @workgroup_size(8, 8)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let full = vec2u(push.full_w, push.full_h);
    if (gid.x >= full.x || gid.y >= full.y) {
        return;
    }
    let center_depth = textureLoad(depth_tex, vec2i(gid.xy), 0).r;
    if (center_depth <= 1e-6) {
        textureStore(ao_full, vec2i(gid.xy), vec4f(1.0, 0.0, 0.0, 0.0));
        return;
    }

    let half = vec2u((push.full_w + 1u) / 2u, (push.full_h + 1u) / 2u);
    let center_uv = (vec2f(gid.xy) + vec2f(0.5)) / vec2f(full);
    let center_world = world_from_depth(center_uv, center_depth);
    // 全分辨率像素中心落回半分辨率网格的位置（象限 2×2 的插值坐标）。
    let low_center = (vec2f(gid.xy) + vec2f(0.5)) * 0.5 - vec2f(0.5);
    let base = floor(low_center);
    let frac = low_center - base;
    let sigma2 = 2.0 * push.sigma * push.sigma;

    var sum = 0.0;
    var weight_sum = 0.0;
    for (var j: i32 = 0; j < 2; j = j + 1) {
        for (var i: i32 = 0; i < 2; i = i + 1) {
            let coord = clamp(vec2i(base) + vec2i(j, i), vec2i(0), vec2i(half) - vec2i(1));
            let bilinear = select(1.0 - frac.x, frac.x, j == 1)
                * select(1.0 - frac.y, frac.y, i == 1);
            // 邻居深度用它在全分辨率深度图上的左上格（半分辨率一格 = 2×2 全分辨率）。
            let spix = clamp(vec2u(coord) * 2u, vec2u(0u), full - vec2u(1u));
            let sd = textureLoad(depth_tex, vec2i(spix), 0).r;
            var w = bilinear;
            if (sd > 1e-6) {
                let suv = (vec2f(spix) + vec2f(0.5)) / vec2f(full);
                let neighbor_world = world_from_depth(suv, sd);
                let delta = neighbor_world - center_world;
                w = w * exp(-dot(delta, delta) / sigma2);
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
        let nearest = clamp(vec2i(low_center + vec2f(0.5)), vec2i(0), vec2i(half) - vec2i(1));
        ao = textureLoad(ao_half_tex, nearest).r;
    }
    textureStore(ao_full, vec2i(gid.xy), vec4f(ao, 0.0, 0.0, 0.0));
}
