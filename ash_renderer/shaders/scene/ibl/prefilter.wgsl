// GGX 预滤波（compute 单入口，逐 mip 派发）：对每个 texel 以它的方向为 N 做 GGX 重要性
// 采样，把 mip0 的辐亮度按 NoL 加权平均后写进目标 mip 级。mip 0 不派发——它是锐利源
// （perceptualRoughness=0 的镜面反射直接采它）。
//
// 采样数逐级下降（128/96/64/48/32/24/16/8，见段施工计划 §0.4）：粗糙级的 lobe 接近
// 半球，样本少也平滑；锐利级需要更多样本才能压噪。
//
// 接口：b0 = mip0 cube 采样（只读 mip0；显式级 0）、b1 = 采样器、b2 = 目标 mip 的
// 2D-array storage 视图（6 层）；push = { perceptual_roughness, sample_count }。
// dispatch = (级宽/8, 级宽/8, 6)。
// #import ibl

struct PushPrefilter {
    /// 目标级的 perceptualRoughness = mip / (mip 数 − 1)。
    perceptual_roughness: f32,
    /// 本级的 GGX 样本数。
    sample_count: u32,
}
var<immediate> push: PushPrefilter;

@group(0) @binding(0) var env_src: texture_cube<f32>;
@group(0) @binding(1) var env_samp: sampler;
@group(0) @binding(2) var dst: texture_storage_2d_array<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<u32>(textureDimensions(dst));
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let uv = (vec2f(gid.xy) + vec2f(0.5)) / vec2f(dims);
    let n = cube_face_direction(gid.z, uv);
    // 坑：重要性采样吃的是 α = p²，不是 perceptualRoughness（直接用 p 会把 lobe 拉宽）
    let a = push.perceptual_roughness * push.perceptual_roughness;
    var sum = vec3f(0.0);
    var weight = 0.0;
    for (var i = 0u; i < push.sample_count; i = i + 1u) {
        let h = importance_sample_ggx(hammersley(i, push.sample_count), n, a);
        let l = reflect(-n, h);
        let no_l = dot(n, l);
        // 背面样本丢弃（GGX 远尾在 NoL≤0 仍有分布，但辐亮度只来自上半球）
        if (no_l > 0.0) {
            sum += textureSampleLevel(env_src, env_samp, l, 0.0).rgb * no_l;
            weight += no_l;
        }
    }
    let c = select(vec3f(0.0), sum / weight, weight > 0.0);
    textureStore(dst, vec2<i32>(gid.xy), i32(gid.z), vec4f(c, 1.0));
}
