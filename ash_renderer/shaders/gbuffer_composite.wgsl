// 延迟光照累积（全屏）：读 G-buffer（albedo/normal/matparam/depth），逐像素查
// 3D cluster 列表（索引域 = 光源常驻表槽号）逐灯累加 Filament BRDF，叠加方向光
//（乘 PCSS 阴影可见性）与环境光，输出 HDR。BRDF 函数族与照抄口径见 common/brdf.wgsl。
// 组织形态与 Filament 的差异（参考篇 §7）：Filament 是 forward 逐片元查 froxel，
// 本 pass 是 deferred 累积；"逐光源累加到 inout color"的循环形状同构。
//
// 模式（frame.mode，FRAME_MODE_*）：
// - 0 Shading：Filament BRDF 光照累积（未知值同样走此分支）
// - 1 Unlit / 2 Normal / 3 Position：反照率 / 世界法线 / 深度重建世界位置直出
// - 4..8 G-buffer 调试仪器：matparam 单通道灰度直出（metallic/roughness/AO/
//   reflectance——材质热改的写后读验收口）+ reverse-Z 深度线性化灰度
//
// 背景判定：reverse-Z 深度清 0 = 远平面 → 输出清屏色。间接光注入项（indirect）
// 的累加位已留在 ambient 旁，目前恒 0。
enable wgpu_binding_array;
// #import frame
// #import cluster_lighting
// #import shadow_ubo
// #import fullscreen

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;
@group(1) @binding(5) var matparam_tex: texture_2d<f32>;
@group(2) @binding(0) var<storage, read> cluster_params: ClusterParams;
@group(2) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(2) @binding(2) var<storage, read> clusters: array<ClusterRecord>;
// shadow 域（set3）：b0 shadow UBO（级联矩阵/分割带/开关旗标）+ b1..4 四级联
// 深度图（D32 textureLoad 自写滤波——non-filterable 路线，不引 compare sampler）。
// shadow 关 = shadow_visibility 恒 1（UBO 旗标）。
@group(3) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(3) @binding(1) var shadow_map0: texture_2d<f32>;
@group(3) @binding(2) var shadow_map1: texture_2d<f32>;
@group(3) @binding(3) var shadow_map2: texture_2d<f32>;
@group(3) @binding(4) var shadow_map3: texture_2d<f32>;

const MODE_SHADING: u32 = 0u;   // FRAME_MODE_LAMBERT（值不变，语义为 BRDF 光照）
const MODE_UNLIT: u32 = 1u;
const MODE_NORMAL: u32 = 2u;
const MODE_POSITION: u32 = 3u;
const MODE_METALLIC: u32 = 4u;
const MODE_ROUGHNESS: u32 = 5u;
const MODE_AO: u32 = 6u;
const MODE_REFLECTANCE: u32 = 7u;
const MODE_DEPTH: u32 = 8u;

// 清屏色（与 host 侧 CLEAR_COLOR 同值——线性域，SRGB 编码在最终 ROP 一次）
const CLEAR_COLOR: vec3f = vec3f(0.02, 0.06, 0.14);

// —— PCSS 软阴影（textureLoad 路线）：
// 按视深选级联；法线抬升后的投影点落出本级盒（含采样核余量）就退到更粗
// 一级，四级都出盒才判无影。深度比较 reverse-Z：更近太阳 = 值更大。
// bias / 法线抬升 / 半影都按米计算（texel_world、inv_span 换算），换级联
// 世界尺度一致。开关旗标（UBO map_size.y）= 全链旁路面。

// 半影宽度 / 遮挡体到接收面距离（米/米），约等于把太阳当成有面积的光源
const SHADOW_PENUMBRA_SCALE: f32 = 0.02;
// 半影半径上限（texel）；同时是出盒判定的采样核余量
const SHADOW_MAX_RADIUS: f32 = 4.0;

fn shadow_map_load(ci: u32, px: vec2<i32>) -> f32 {
    let last = i32(shadow.map_size.x) - 1;
    let p = clamp(px, vec2<i32>(0), vec2<i32>(last));
    switch ci {
        case 0u: { return textureLoad(shadow_map0, p, 0).r; }
        case 1u: { return textureLoad(shadow_map1, p, 0).r; }
        case 2u: { return textureLoad(shadow_map2, p, 0).r; }
        default: { return textureLoad(shadow_map3, p, 0).r; }
    }
}

// 单级联 PCSS：px = 接收点 texel 坐标，own = 接收点 reverse-Z 光深度。
// 斜面上采样点离中心越远，表面自身在图里越"高"，bias 按 tap 距离 × 坡度加。
fn shadow_pcss(ci: u32, px: vec2f, own: f32, no_l: f32) -> f32 {
    let texel_world = shadow.texel_world[ci];
    let inv_span = shadow.inv_span[ci];
    let tan_t = sqrt(max(1.0 - no_l * no_l, 0.0)) / max(no_l, 0.25);
    let bias_m = texel_world * (1.0 + tan_t) + 0.01;
    let slope_m = texel_world * tan_t;
    let center = vec2<i32>(px);
    // —— blocker search（3×3，步长 2 texel）
    var blocker_sum = 0.0;
    var blocker_count = 0.0;
    for (var dy = -1i; dy <= 1i; dy = dy + 1i) {
        for (var dx = -1i; dx <= 1i; dx = dx + 1i) {
            let o = vec2<i32>(dx, dy) * 2i;
            let d = shadow_map_load(ci, center + o);
            let tap_bias = (bias_m + slope_m * length(vec2f(o))) * inv_span;
            if (d > own + tap_bias) {
                blocker_sum = blocker_sum + d;
                blocker_count = blocker_count + 1.0;
            }
        }
    }
    if (blocker_count == 0.0) {
        return 1.0;
    }
    // —— 半影半径：遮挡体到接收面的米距离 × 光源尺度 → texel
    let blocker_m = (blocker_sum / blocker_count - own) / inv_span;
    let radius = clamp(blocker_m * SHADOW_PENUMBRA_SCALE / texel_world, 1.0, SHADOW_MAX_RADIUS);
    // —— PCF 5×5，覆盖 ±radius texel
    var lit = 0.0;
    for (var dy = -2i; dy <= 2i; dy = dy + 1i) {
        for (var dx = -2i; dx <= 2i; dx = dx + 1i) {
            let off = vec2f(f32(dx), f32(dy)) * (radius * 0.5);
            let d = shadow_map_load(ci, vec2<i32>(px + off));
            let tap_bias = (bias_m + slope_m * length(off)) * inv_span;
            lit = lit + select(1.0, 0.0, d > own + tap_bias);
        }
    }
    return lit / 25.0;
}

fn shadow_visibility(world_pos: vec3f, n: vec3f) -> f32 {
    if (shadow.map_size.y < 0.5) {
        return 1.0;
    }
    let view_depth = dot(world_pos - shadow.camera_pos.xyz, shadow.camera_forward.xyz);
    var start = 4u;
    for (var c = 0u; c < 4u; c = c + 1u) {
        if (view_depth <= shadow.splits[c]) {
            start = c;
            break;
        }
    }
    let l = safe_light_dir(frame.dir_to_light.xyz);
    let no_l = saturate(dot(n, l));
    let size = shadow.map_size.x;
    let margin = SHADOW_MAX_RADIUS + 3.0;
    for (var c = start; c < 4u; c = c + 1u) {
        // 法线抬升（防自遮蔽）：1~3 texel，掠射面抬得多
        let lifted = world_pos + n * (shadow.texel_world[c] * (1.0 + 2.0 * (1.0 - no_l)));
        let cs = shadow.cascade_view_proj[c] * vec4f(lifted, 1.0);
        let ndc = cs.xyz / cs.w;
        let px = vec2f(ndc.x * 0.5 + 0.5, 0.5 - 0.5 * ndc.y) * size;
        let inside = all(px >= vec2f(margin)) && all(px <= vec2f(size - margin))
            && ndc.z >= 0.0 && ndc.z <= 1.0;
        if (inside) {
            return shadow_pcss(c, px, ndc.z, no_l);
        }
    }
    return 1.0;
}

// 深度反投影：uv → shader-NDC（uv.y = 0.5 - 0.5·ndc.y 的逆变换）→ clip → world
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let albedo = textureSampleLevel(albedo_tex, gbuf_sampler, in.uv, 0.0);
    let depth = textureSampleLevel(depth_tex, gbuf_sampler, in.uv, 0.0);
    // 背景：reverse-Z 清 0 = 远平面，无几何写深度——输出清屏色
    if (depth.r <= 1e-6) {
        return vec4f(CLEAR_COLOR, 1.0);
    }
    if frame.mode == MODE_UNLIT {
        return vec4f(albedo.rgb, 1.0);
    }
    let n = normalize(textureSampleLevel(normal_tex, gbuf_sampler, in.uv, 0.0).xyz);
    if frame.mode == MODE_NORMAL {
        return vec4f(n * 0.5 + vec3f(0.5), 1.0);
    }
    if frame.mode == MODE_POSITION {
        return vec4f(world_from_depth(in.uv, depth.r) * 0.0625 + vec3f(0.5), 1.0);
    }
    // —— G-buffer 调试仪器：matparam 单通道灰度直出。8bit 量化面如实呈现
    //（写入端 clamp/域换算一并可见）；背景已在上方清屏色早退。
    if (frame.mode == MODE_METALLIC || frame.mode == MODE_ROUGHNESS
        || frame.mode == MODE_AO || frame.mode == MODE_REFLECTANCE) {
        let m = textureSampleLevel(matparam_tex, gbuf_sampler, in.uv, 0.0);
        let c = select(
            select(
                select(m.r, m.g, frame.mode == MODE_AO),
                m.b, frame.mode == MODE_ROUGHNESS),
            m.a, frame.mode == MODE_REFLECTANCE);
        return vec4f(vec3f(c), 1.0);
    }
    if frame.mode == MODE_DEPTH {
        // reverse-Z 深度线性化。坑点：片段阶段 @builtin(position).w = 1/clip.w，
        // 不能直读当视深——走反投影重建世界位置，再经 view_proj 取 clip.w =
        // 视空间深度（透视 w = 正距离），除 far 归一。far 取 cluster 参数面（同帧同相机）。
        let world = world_from_depth(in.uv, depth.r);
        let view_depth = (frame.view_proj * vec4f(world, 1.0)).w;
        let t = saturate(view_depth / max(cluster_params.far, 1e-4));
        return vec4f(vec3f(t), 1.0);
    }

    // —— Shading（mode 0 及未知值兜底）：Filament BRDF 光照累积
    // matparam：r = metallic / g = AO / b = perceptualRoughness / a = reflectance 原值
    let matparam = textureSampleLevel(matparam_tex, gbuf_sampler, in.uv, 0.0);
    let world_pos = world_from_depth(in.uv, depth.r);
    let V = normalize(frame.camera_pos.xyz - world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    let pixel = getStandardPixelParams(albedo.rgb, matparam.r, matparam.b, matparam.a);
    // AO 乘光源贡献尾部（参考篇 §4 occlusion 语义）；环境 AO（multiBounceAO）
    // 归间接光
    let occlusion = matparam.g;

    // 点光源先累加、方向光后加（累加顺序即浮点结果，保持不变）
    var light_sum = accumulate_cluster_lights(
        vec3f(0.0), cluster_params, in.clip.xy, pixel, world_pos, V, n, NoV, occlusion);

    // —— 方向光（set0 UBO 单光，不进灯表）：Filament 方向光形状 = NoL ×
    // colorIntensity，无 attenuation。
    let l = safe_light_dir(frame.dir_to_light.xyz);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        // 方向光项乘阴影可见性（透明层/环境项不接影）
        let shadow = shadow_visibility(world_pos, n);
        // 强度已折进 rgb（bevy 同款 illuminance 预乘，w 传 1.0——Filament 的
        // colorIntensity.w 预曝光位由 rgb 内含强度承担）
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, occlusion)
            * shadow;
    }

    // 环境光项在光源循环外（参考篇 §6.4）：albedo × ambient
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    // 间接光注入项：空接口，恒 0
    let indirect = vec3f(0.0);
    return vec4f((light_sum + ambient + indirect) * EXPOSURE, 1.0);
}
