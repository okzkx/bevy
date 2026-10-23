// Bloom 降采样（全屏，dual-Kawase down）：13 tap 双线性加权（中心 4 +
// 角 4 + 边 4 + 自身），半分辨率逐级——输入为上一级目标。
//
// 接口：set0 = b0 上一级纹理 + b2 采样器（BloomTargets set_layout_tex——
// binding 编号随域常量 TEX0=0/SAMPLER=2，与 up 集同族对齐）。
// #import fullscreen

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var src_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // 源纹素尺寸（半分辨率目标按 2:1 降——偏移 = 目标 texel × 1 换算源空间）
    let dim = vec2f(textureDimensions(src_tex));
    let px = 1.0 / dim;
    let o = px;
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
