// 8.4 present tonemap（全屏，接缝正式启用）：HDR 中间目标 + Bloom 叠加 →
// ACES 近似 tone mapping → swapchain。输出编码恰好一次的落点不变：本 shader
// 全程线性，swapchain 的 SRGB view 在 ROP 写出时做唯一一次编码（3.4 拍板）。
//
// ACES 曲线选型（立案钉明）：Narkowicz 2015 拟合式（常量 2.51/0.03/2.43/0.59/
// 0.14，出处 "ACES Filmic Tone Mapping Curve" 摘要）——立案允许的近似族；
// 物理版（Filament RRT+ODT fitted/LUT）留步骤 9 IBL 时一并评估。
//
// 接口：set0 = 帧 UBO（mode 旁路面）；set1 = G-buffer 采样集（b2 hdr）；set2 =
// bloom 参数集（b0 up0 / b1 合法闲置 / b2 采样器 / b3 参数 UBO）。
// debug 仪器旁路：mode != filament(0) 时 HDR 内是直出可视化——跳过 bloom
// 叠加与 ACES（写后读面失真即仪器失真）。

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(2) var hdr_tex: texture_2d<f32>;
@group(1) @binding(4) var blit_sampler: sampler;
@group(2) @binding(0) var bloom_tex: texture_2d<f32>;
@group(2) @binding(2) var bloom_sampler: sampler;
@group(2) @binding(3) var<uniform> bloom_params: BloomParams;

struct FrameUniforms {
    view_proj: mat4x4f,
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
}

struct BloomParams {
    strength: f32,
    enabled: f32,
    tonemap: f32,
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: VsOut;
    out.clip = vec4f(p[vi], 0.0, 1.0);
    out.uv = vec2f(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return out;
}

// Narkowicz ACES 拟合（线性域输入/输出，编码在 SRGB view ROP 恰好一次）
fn aces(x: vec3f) -> vec3f {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3f(0.0), vec3f(1.0));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let hdr = textureSampleLevel(hdr_tex, blit_sampler, in.uv, 0.0).rgb;
    if (frame.mode != 0u || bloom_params.tonemap < 0.5) {
        // debug 仪器直出 / tonemap 关（A/B 的 B 面）——HDR 内容原样
        return vec4f(hdr, 1.0);
    }
    var bloom = vec3f(0.0);
    if (bloom_params.enabled > 0.5) {
        bloom = textureSampleLevel(bloom_tex, bloom_sampler, in.uv, 0.0).rgb
            * bloom_params.strength;
    }
    return vec4f(aces(hdr + bloom), 1.0);
}
