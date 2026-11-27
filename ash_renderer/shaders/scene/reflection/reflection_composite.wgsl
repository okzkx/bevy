// 反射合成（10.3，全分辨率全屏 pass）。
//
// 读 `hdr_copy`（合成原图）与半分辨率反射图，写回 HDR 目标：
//   `HDR = hdr_copy + w · (命中色 − 原本的 IBL 高光 · EXPOSURE)`
// - `命中色` 来自 HDR 拷贝，已含曝光；`ibl_specular_from` 重算的正是合成端加进 HDR 的那一项
//   （* EXPOSURE 后才进 HDR），所以「减掉它、换成命中色」不会双重计环境光。
// - `w = 0`（天窗/粗糙面/未命中/边缘淡出到 0）时只是加 0，IEEE 精确恒等。
// - IBL 关（`frame.ibl_enabled == 0`）时不减任何东西（合成端那时也没有 IBL 高光）。
// - 不做混合：全屏管线写满每像素，所以 `clear: None` 也不需要读旧 HDR。

// #import frame
// #import fullscreen
// #import ibl
// #import brdf

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;
@group(1) @binding(5) var matparam_tex: texture_2d<f32>;
@group(2) @binding(0) var env_cube: texture_cube<f32>;
@group(2) @binding(1) var ibl_sampler: sampler;
@group(2) @binding(2) var dfg_lut: texture_2d<f32>;
@group(3) @binding(0) var reflection: texture_2d<f32>;
@group(3) @binding(1) var hdr_copy: texture_2d<f32>;

// 深度反投影（与 gbuffer_composite / ssao / ssr 同式；各入口自己带一份，公共片段里
// 不放它，避免 import 顺序影响 `frame` 的可见性）。
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let base = textureSampleLevel(hdr_copy, gbuf_sampler, in.uv, 0.0);
    // 半分辨率反射图手写双线性上采样：storage 图停在 GENERAL 布局，不能配 sampler，
    // 四邻域 textureLoad 加权即可消掉半分辨率的阶梯（判定 4 的「无拉丝/块状」靠它）。
    let half = vec2f(textureDimensions(reflection));
    let texel = in.uv * half - vec2f(0.5);
    let i0 = vec2i(floor(texel));
    let f = texel - floor(texel);
    let hi = vec2i(half) - vec2i(1);
    let r00 = textureLoad(reflection, clamp(i0, vec2i(0), hi), 0);
    let r10 = textureLoad(reflection, clamp(i0 + vec2i(1, 0), vec2i(0), hi), 0);
    let r01 = textureLoad(reflection, clamp(i0 + vec2i(0, 1), vec2i(0), hi), 0);
    let r11 = textureLoad(reflection, clamp(i0 + vec2i(1, 1), vec2i(0), hi), 0);
    let r = mix(mix(r00, r10, f.x), mix(r01, r11, f.x), f.y);
    let w = clamp(r.a, 0.0, 1.0);
    // 背景（天空）不参与：深度清 0 的像素原样输出，避免天空上叠反射
    let depth = textureSampleLevel(depth_tex, gbuf_sampler, in.uv, 0.0).r;
    if (depth <= 1e-6 || w <= 0.0) {
        return base;
    }
    let albedo = textureSampleLevel(albedo_tex, gbuf_sampler, in.uv, 0.0);
    let matparam = textureSampleLevel(matparam_tex, gbuf_sampler, in.uv, 0.0);
    let pixel = getStandardPixelParams(albedo.rgb, matparam.r, matparam.b, matparam.a);
    let world = world_from_depth(in.uv, depth);
    let v = normalize(frame.camera_pos.xyz - world);
    let n_raw = normalize(textureSampleLevel(normal_tex, gbuf_sampler, in.uv, 0.0).xyz);
    let n = ibl_normal(n_raw, v);
    let no_v = clampNoV(saturate(dot(n, v)));
    let p_ibl = ibl_roughness(n, matparam.b);
    var spec = vec3f(0.0);
    if (frame.ibl_enabled != 0u) {
        spec = ibl_specular_from(
            env_cube, ibl_sampler, dfg_lut, n, v, no_v, p_ibl, pixel.f0, frame.ibl_intensity);
    }
    // 反射替换该像素原本的 IBL 高光（而不是叠加），命中色与 spec 都在同一曝光域
    return vec4f(base.rgb + w * (r.rgb - spec * EXPOSURE), base.a);
}
