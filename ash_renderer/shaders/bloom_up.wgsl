// Bloom 升采样（全屏，dual-Kawase up）：up[i] = tent(up[i+1]) + down[i]
// ——4 tap 双线性 tent 近似 + 对应降采样级直加（shader 内求和，无混合状态）。
//
// 接口：set0 = b0 up 源（最小级时 = down[N-1]）+ b1 down 对应级 + b2 采样器
// + b3 参数 UBO（BloomTargets set_layout_up；b1/b3 在最小级直通场景合法在用）。
// #import fullscreen

@group(0) @binding(0) var up_tex: texture_2d<f32>;
@group(0) @binding(1) var down_tex: texture_2d<f32>;
@group(0) @binding(2) var src_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let dim = vec2f(textureDimensions(up_tex));
    let o = 1.0 / dim;
    // 4 tap 双线性 tent（角点均权 ≈ 3×3 高斯的廉价形态）
    var t = textureSampleLevel(up_tex, src_sampler, in.uv + vec2f(-o.x, -o.y), 0.0).rgb;
    t += textureSampleLevel(up_tex, src_sampler, in.uv + vec2f(o.x, -o.y), 0.0).rgb;
    t += textureSampleLevel(up_tex, src_sampler, in.uv + vec2f(-o.x, o.y), 0.0).rgb;
    t += textureSampleLevel(up_tex, src_sampler, in.uv + vec2f(o.x, o.y), 0.0).rgb;
    let up = t / 4.0;
    let down = textureSampleLevel(down_tex, src_sampler, in.uv, 0.0).rgb;
    return vec4f(up + down, 1.0);
}
