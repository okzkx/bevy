// 1/4 屏幕去噪：按参考子像素的真实几何加权，先解码系数再混合。
// RGB 是有效覆盖上的条件辐照度；alpha 独立表示覆盖，不能把无效零值混入 RGB 后再乘 alpha。
// L1.a 保存中心参考子像素编号，随去噪保留，供下一遍和上采样定位 G-buffer。
// #import frame
// #import gi_cell
// #import gi_compression

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var scgi_l0_in: texture_storage_2d<rgba16float, read>;
@group(2) @binding(1) var scgi_l1_in: texture_storage_2d<rgba8unorm, read>;
@group(2) @binding(2) var scgi_l0_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(3) var scgi_l1_out: texture_storage_2d<rgba8unorm, write>;

struct PushDenoise {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    step: u32,
    reserved: u32,
}
var<immediate> push: PushDenoise;

fn guide_pixel(low: vec2i, phase: f32) -> vec2i {
    return clamp(low * 4 + scgi_guide_offset(phase), vec2i(0), vec2i(i32(push.full_w) - 1, i32(push.full_h) - 1));
}

fn guide_position(pixel: vec2i, depth: f32) -> vec3f {
    let uv = (vec2f(pixel) + vec2f(0.5)) / vec2f(f32(push.full_w), f32(push.full_h));
    return world_from_depth(uv, depth, frame.inv_view_proj);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    if (gid.x >= push.low_w * push.low_h) {
        return;
    }
    let low = vec2i(i32(gid.x % push.low_w), i32(gid.x / push.low_w));
    let center_l1 = textureLoad(scgi_l1_in, low);
    let center_pixel = guide_pixel(low, center_l1.w);
    let center_depth = textureLoad(depth_tex, center_pixel, 0).r;
    let center_normal_raw = textureLoad(normal_tex, center_pixel, 0).xyz;
    if (center_depth <= 0.0 || dot(center_normal_raw, center_normal_raw) < 1e-8) {
        textureStore(scgi_l0_out, low, vec4f(0.0));
        textureStore(scgi_l1_out, low, vec4f(0.5, 0.5, 0.5, center_l1.w));
        return;
    }
    let center_position = guide_position(center_pixel, center_depth);
    let center_normal = scgi_stable_guide_normal(
        depth_tex, center_pixel, vec2u(push.full_w, push.full_h), center_position, center_depth,
        center_normal_raw, frame.inv_view_proj, frame.camera_pos.xyz,
    );
    var acc_l0 = vec3f(0.0);
    var acc_l1x = vec3f(0.0);
    var acc_l1y = vec3f(0.0);
    var acc_l1z = vec3f(0.0);
    var valid_weight_sum = 0.0;
    var attempted_weight = 0.0;
    let step = i32(max(push.step, 1u));
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let coord = low + vec2i(dx, dy) * step;
            if (coord.x < 0 || coord.y < 0 || coord.x >= i32(push.low_w) || coord.y >= i32(push.low_h)) {
                continue;
            }
            let gauss = exp(-f32(dx * dx + dy * dy) / 2.0);
            let sample_l0 = textureLoad(scgi_l0_in, coord);
            let sample_l1 = textureLoad(scgi_l1_in, coord);
            let pixel = guide_pixel(coord, sample_l1.w);
            let depth = textureLoad(depth_tex, pixel, 0).r;
            let normal_raw = textureLoad(normal_tex, pixel, 0).xyz;
            if (depth <= 0.0 || dot(normal_raw, normal_raw) < 1e-8) {
                continue;
            }
            let position = guide_position(pixel, depth);
            let normal = scgi_stable_guide_normal(
                depth_tex, pixel, vec2u(push.full_w, push.full_h), position, depth,
                normal_raw, frame.inv_view_proj, frame.camera_pos.xyz,
            );
            let geometry_weight = gauss * scgi_geometry_weight(
                center_position, center_normal, position, normal, frame.camera_pos.xyz,
            );
            // 几何被拒不是缓存缺失；同表面 a=0 则仍须进入覆盖率分母。
            attempted_weight = attempted_weight + geometry_weight;
            let weight = geometry_weight * clamp(sample_l0.w, 0.0, 1.0);
            if (weight <= 0.0) {
                continue;
            }
            acc_l0 = acc_l0 + sample_l0.xyz * weight;
            acc_l1x = acc_l1x + l1_decode_axis(sample_l1.x, sample_l0.xyz) * weight;
            acc_l1y = acc_l1y + l1_decode_axis(sample_l1.y, sample_l0.xyz) * weight;
            acc_l1z = acc_l1z + l1_decode_axis(sample_l1.z, sample_l0.xyz) * weight;
            valid_weight_sum = valid_weight_sum + weight;
        }
    }
    var out_l0 = vec3f(0.0);
    var out_ratio = vec3f(0.5);
    var coverage = 0.0;
    if (valid_weight_sum > 0.0) {
        out_l0 = acc_l0 / valid_weight_sum;
        out_ratio = vec3f(
            l1_encode_axis(acc_l1x / valid_weight_sum, out_l0),
            l1_encode_axis(acc_l1y / valid_weight_sum, out_l0),
            l1_encode_axis(acc_l1z / valid_weight_sum, out_l0),
        );
        coverage = clamp(valid_weight_sum / attempted_weight, 0.0, 1.0);
    }
    textureStore(scgi_l0_out, low, vec4f(out_l0, coverage));
    textureStore(scgi_l1_out, low, vec4f(out_ratio, center_l1.w));
}
