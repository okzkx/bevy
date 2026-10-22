// 8.4 Bloom prefilter（全屏）：HDR 线性输入 → 亮度阈值 + soft knee 提亮部
// ——Kawase 双滤波链的源头级（半分辨率目标）。
//
// 接口：set0 = G-buffer 采样集（b2 hdr / b4 采样器——合成/present 共用布局，
// 其余 binding 合法闲置）。阈值常量：软膝起点 1.0（HDR 域 >1 即光晕源）、
// 膝宽 0.5（立案钉明的起步值，读图调）。

@group(0) @binding(2) var hdr_tex: texture_2d<f32>;
@group(0) @binding(4) var blit_sampler: sampler;

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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let hdr = textureSampleLevel(hdr_tex, blit_sampler, in.uv, 0.0).rgb;
    let luma = dot(hdr, vec3f(0.2126, 0.7152, 0.0722));
    // soft knee：膝下全保、膝上线性压到阈值
    let knee = 0.5;
    let soft = clamp(luma - 1.0 + knee, 0.0, 2.0 * knee);
    let contrib = max(soft * soft / (4.0 * knee + 1e-4), luma - 1.0);
    let scale = select(0.0, contrib / max(luma, 1e-4), luma > 1e-4);
    return vec4f(hdr * scale, 1.0);
}
