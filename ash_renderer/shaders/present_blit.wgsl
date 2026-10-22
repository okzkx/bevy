// 8.1 present blit（全屏）：把 HDR 中间目标采样写进 swapchain——渲染目标从
// "swapchain 直出"改为"中间目标 + 终点 blit"的最后一跳（步骤 8 §0：8.4 起
// tone mapping 插在中间目标与 swapchain 之间，本 pass 是那个接缝的当前形态）。
// 输出编码恰好一次：HDR 中间目标是线性域数据，swapchain 的 SRGB view 在
// ROP 写出时做唯一一次编码——本 shader 全程线性，不做任何编码。
//
// 接口：set0 = G-buffer 采样集（b2 hdr / b4 采样器，gbuffer.rs 布局契约；
// 该布局其余 binding 在本管线合法闲置）。

@group(0) @binding(2) var hdr_tex: texture_2d<f32>;
@group(0) @binding(4) var blit_sampler: sampler;

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // 全屏三角形；uv.y 翻转适配 naga 坐标注入（同 gbuffer_composite 注释）
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: VsOut;
    out.clip = vec4f(p[vi], 0.0, 1.0);
    out.uv = vec2f(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let hdr = textureSampleLevel(hdr_tex, blit_sampler, in.uv, 0.0);
    return vec4f(hdr.rgb, 1.0);
}
