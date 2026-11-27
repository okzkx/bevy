// 半分辨率 SSAO（10.2.2）：读 G-buffer 的全分辨率深度/法线，在每个半分辨率像素上做
// 世界空间半球采样，用「期望视深 vs 实际视深」判遮挡，输出半分辨率 AO（r32float）。
//
// 口径：
// - 深度/法线按全分辨率坐标 (2x, 2y) 取——半分辨率像素左上角那一个全分辨率格子；
//   关档时合成端不读这张图，所以这半像素偏移只影响质量判定，不影响逐字节回归。
// - 4 个方向 × steps 步：方向角用 IGN（像素 + 帧号）旋转，4 个方向均匀铺开 90°；
//   仰角按 z = (i + 0.5) / steps 铺满半球，半径 r = radius_m · (i + 1) / steps。
// - 命中判定：采样点投影回屏幕读场景视深，`expected_w > actual_w + bias` 才算遮蔽。
//   视深取 view_proj 的 w 行（透视下即正的距离），与调试图 MODE_DEPTH 同口径（R3）。
// - 再加一层距离带（thickness）上限：`expected_w - actual_w < 2·radius_m` 才算遮蔽。
//   没有这层时，掠射面上的采样点（天花板/地板近切线）与同面邻点的投影视深差可达
//   数米，会被误判成遮挡，整片画面泛黑——2026-11-27 实测踩到过（默认机位 24% 像素
//   AO<0.8），补上距离带后回到接触区局部变暗。
// - 天空（reverse-Z 深度 <= 1e-6）与投影到屏幕外的采样点直接跳过。
//
// 输出：ao = saturate(1 - intensity · occluded / (4 · steps))。关档不录本 dispatch，
// 合成端绑 1×1 白图，乘 1.0 逐字节恒等（R5）。
// #import frame

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var ao_half: texture_storage_2d<r32float, write>;

struct PushSsao {
    full_w: u32,
    full_h: u32,
    radius_m: f32,
    bias: f32,
    intensity: f32,
    steps: u32,
    frame: u32,
    // bit0 = 全分辨率诊断模式（判定线 4 的半/全对照）：按全分辨率像素中心采样、
    // 写进绑在 b0 的全分辨率图；正常渲染为 0（半分辨率 + 上采样）。
    flags: u32,
}

var<immediate> push: PushSsao;

const PI: f32 = 3.141592653589793;

// 深度反投影：uv → shader-NDC → clip → world（与 gbuffer_composite 同式）。
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

// 视深：透视投影下 clip.w 就是沿视线方向的正距离（与调试图 MODE_DEPTH 同口径）。
fn view_depth(world: vec3f) -> f32 {
    return (frame.view_proj * vec4f(world, 1.0)).w;
}

// Interleaved Gradient Noise：同帧内相邻像素的旋转角不同，把 4 方向×steps 的采样
// 图案打散成近似均匀的半球覆盖（不做时域累积，见边界）。
fn ign(pixel: vec2u, frame_index: u32) -> f32 {
    let p = vec2f(pixel) + vec2f(0.5) + f32(frame_index & 63u);
    return fract(52.9829189 * fract(0.06711056 * p.x + 0.00583715 * p.y));
}

fn basis_from_normal(n: vec3f) -> mat3x3f {
    let up = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(n.y) > 0.99);
    let t = normalize(cross(up, n));
    let b = cross(n, t);
    return mat3x3f(t, b, n);
}

@compute @workgroup_size(8, 8)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let full = vec2u(push.full_w, push.full_h);
    let full_res = (push.flags & 1u) != 0u;
    let half = vec2u((push.full_w + 1u) / 2u, (push.full_h + 1u) / 2u);
    let bound = select(half, full, full_res);
    if (gid.x >= bound.x || gid.y >= bound.y) {
        return;
    }
    let pix = select(gid.xy * 2u, gid.xy, full_res);
    let depth = textureLoad(depth_tex, vec2i(pix), 0).r;
    // reverse-Z 的 0 = 远平面（天空）：不遮蔽。
    if (depth <= 1e-6) {
        textureStore(ao_half, vec2i(gid.xy), vec4f(1.0, 0.0, 0.0, 0.0));
        return;
    }

    let uv = (vec2f(pix) + vec2f(0.5)) / vec2f(full);
    let world = world_from_depth(uv, depth);
    let n = normalize(textureLoad(normal_tex, vec2i(pix), 0).xyz);
    let basis = basis_from_normal(n);
    let seed = ign(gid.xy, push.frame) * 2.0 * PI;
    let steps = max(push.steps, 1u);

    var occluded = 0u;
    var i = 0u;
    loop {
        if (i >= steps) {
            break;
        }
        let t = (f32(i) + 0.5) / f32(steps);
        let sin_t = sqrt(max(1.0 - t * t, 0.0));
        let r = push.radius_m * (f32(i) + 1.0) / f32(steps);
        for (var k = 0u; k < 4u; k = k + 1u) {
            let a = seed + f32(k) * (PI * 0.5);
            let dir = basis * vec3f(cos(a) * sin_t, sin(a) * sin_t, t);
            let sample_world = world + dir * r;
            let clip = frame.view_proj * vec4f(sample_world, 1.0);
            if (clip.w <= 0.0) {
                continue;
            }
            let ndc = clip.xy / clip.w;
            let suv = vec2f(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
            if (suv.x < 0.0 || suv.x >= 1.0 || suv.y < 0.0 || suv.y >= 1.0) {
                continue;
            }
            let spix = vec2i(clamp(suv * vec2f(full), vec2f(0.0), vec2f(full) - vec2f(1.0)));
            let sd = textureLoad(depth_tex, spix, 0).r;
            if (sd <= 1e-6) {
                continue;
            }
            let scene_world = world_from_depth((vec2f(spix) + vec2f(0.5)) / vec2f(full), sd);
            let sdepth = view_depth(scene_world);
            let delta = clip.w - sdepth;
            if (delta > push.bias && delta < 2.0 * push.radius_m) {
                occluded = occluded + 1u;
            }
        }
        i = i + 1u;
    }

    let occ = f32(occluded) / f32(4u * steps);
    let ao = saturate(1.0 - push.intensity * occ);
    textureStore(ao_half, vec2i(gid.xy), vec4f(ao, 0.0, 0.0, 0.0));
}
