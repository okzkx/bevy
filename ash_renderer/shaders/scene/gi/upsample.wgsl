// 全分辨率上采样：L1.a 定位参考几何，RGBL1 系数按有效覆盖同权重混合。
// 无有效贡献时 alpha=0 回 IBL；不能用邻居最大 alpha 为被拒绝的零 RGB 宣称完全覆盖。
// 求值法线与 IBL 一致，翻到视向半球；仅在此处把系数求值成物理辐照度。
// #import frame
// #import gi_cell
// #import gi_compression

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var scgi_l0_in: texture_storage_2d<rgba16float, read>;
@group(2) @binding(1) var scgi_l1_in: texture_storage_2d<rgba8unorm, read>;
@group(2) @binding(4) var scgi_irradiance_out: texture_storage_2d<rgba16float, write>;

struct PushUpsample {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    mode: u32,
    reserved: u32,
}
var<immediate> push: PushUpsample;

fn low_texel_coord(low: vec2i) -> vec2i {
    return clamp(low, vec2i(0), vec2i(i32(push.low_w) - 1, i32(push.low_h) - 1));
}

fn guide_position(pixel: vec2i, depth: f32) -> vec3f {
    let uv = (vec2f(pixel) + vec2f(0.5)) / vec2f(f32(push.full_w), f32(push.full_h));
    return world_from_depth(uv, depth, frame.inv_view_proj);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    if (gid.x >= push.full_w * push.full_h) {
        return;
    }
    let pixel = vec2i(i32(gid.x % push.full_w), i32(gid.x / push.full_w));
    let depth = textureLoad(depth_tex, pixel, 0).r;
    let normal_raw = textureLoad(normal_tex, pixel, 0).xyz;
    if (depth <= 0.0 || dot(normal_raw, normal_raw) < 1e-8) {
        textureStore(scgi_irradiance_out, pixel, vec4f(0.0));
        return;
    }
    let position = guide_position(pixel, depth);
    let normal = scgi_face_normal(normal_raw, position, frame.camera_pos.xyz);
    let geometric_normal = scgi_stable_guide_normal(
        depth_tex, pixel, vec2u(push.full_w, push.full_h), position, depth,
        normal_raw, frame.inv_view_proj, frame.camera_pos.xyz,
    );
    let low_pos = (vec2f(pixel) + vec2f(0.5)) * 0.25 - vec2f(0.5);
    let base = vec2i(floor(low_pos));
    let frac = low_pos - vec2f(base);
    let texel_of = array<vec2i, 4>(vec2i(0, 0), vec2i(1, 0), vec2i(0, 1), vec2i(1, 1));
    let bilinear = array<f32, 4>(
        (1.0 - frac.x) * (1.0 - frac.y),
        frac.x * (1.0 - frac.y),
        (1.0 - frac.x) * frac.y,
        frac.x * frac.y,
    );
    var acc_l0 = vec3f(0.0);
    var acc_l1x = vec3f(0.0);
    var acc_l1y = vec3f(0.0);
    var acc_l1z = vec3f(0.0);
    var valid_weight_sum = 0.0;
    var attempted_weight = 0.0;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let low = low_texel_coord(base + texel_of[i]);
        let sample_l0 = textureLoad(scgi_l0_in, low);
        let sample_l1 = textureLoad(scgi_l1_in, low);
        let neighbor_pixel = clamp(low * 4 + scgi_guide_offset(sample_l1.w), vec2i(0), vec2i(i32(push.full_w) - 1, i32(push.full_h) - 1));
        let neighbor_depth = textureLoad(depth_tex, neighbor_pixel, 0).r;
        let neighbor_normal_raw = textureLoad(normal_tex, neighbor_pixel, 0).xyz;
        if (neighbor_depth <= 0.0 || dot(neighbor_normal_raw, neighbor_normal_raw) < 1e-8) {
            continue;
        }
        let neighbor_position = guide_position(neighbor_pixel, neighbor_depth);
        let neighbor_normal = scgi_stable_guide_normal(
            depth_tex, neighbor_pixel, vec2u(push.full_w, push.full_h), neighbor_position, neighbor_depth,
            neighbor_normal_raw, frame.inv_view_proj, frame.camera_pos.xyz,
        );
        var weight = bilinear[i];
        if (push.mode == 0u) {
            weight = weight * scgi_geometry_weight(position, geometric_normal, neighbor_position, neighbor_normal, frame.camera_pos.xyz);
        }
        // 同几何 a=0 保留分母，被双边拒绝的其他表面不降低本表面的覆盖。
        attempted_weight = attempted_weight + weight;
        weight = weight * clamp(sample_l0.w, 0.0, 1.0);
        if (weight <= 0.0) {
            continue;
        }
        acc_l0 = acc_l0 + sample_l0.xyz * weight;
        acc_l1x = acc_l1x + l1_decode_axis(sample_l1.x, sample_l0.xyz) * weight;
        acc_l1y = acc_l1y + l1_decode_axis(sample_l1.y, sample_l0.xyz) * weight;
        acc_l1z = acc_l1z + l1_decode_axis(sample_l1.z, sample_l0.xyz) * weight;
        valid_weight_sum = valid_weight_sum + weight;
    }
    var irradiance = vec3f(0.0);
    var coverage = 0.0;
    if (valid_weight_sum > 0.0) {
        let l0 = acc_l0 / valid_weight_sum;
        let l1x = acc_l1x / valid_weight_sum;
        let l1y = acc_l1y / valid_weight_sum;
        let l1z = acc_l1z / valid_weight_sum;
        irradiance = max(scgi_evaluate(l0, l1x, l1y, l1z, normal), vec3f(0.0));
        coverage = clamp(valid_weight_sum / attempted_weight, 0.0, 1.0);
    }
    textureStore(scgi_irradiance_out, pixel, vec4f(irradiance, coverage));
}
