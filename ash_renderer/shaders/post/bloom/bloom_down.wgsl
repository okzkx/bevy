// Bloom 降采样（全屏，dual-Kawase down）：9 tap 双线性加权（中心 ×4 + 4 个角 +
// 4 个边，总权 12），逐级半分辨率，输入为上一级目标。
//
// 接口：set0 = b0 上一级纹理 + b2 采样器（BloomTargets 的降采样集，binding 编号
// 与升采样集对齐）。
// #import fullscreen

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var src_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // 偏移 = 一个源纹素
    let dim = vec2f(textureDimensions(src_tex));
    let o = 1.0 / dim;
    var c = textureSampleLevel(src_tex, src_sampler, in.uv, 0.0).rgb * 4.0;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(-o.x, -o.y), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(o.x, -o.y), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(-o.x, o.y), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(o.x, o.y), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(-2.0 * o.x, 0.0), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(2.0 * o.x, 0.0), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(0.0, -2.0 * o.y), 0.0).rgb;
    c += textureSampleLevel(src_tex, src_sampler, in.uv + vec2f(0.0, 2.0 * o.y), 0.0).rgb;
    return vec4f(c / 12.0, 1.0);
}
