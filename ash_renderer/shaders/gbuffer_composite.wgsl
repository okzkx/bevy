// 8.2 延迟光照累积（全屏）：读 G-buffer（albedo/normal/matparam/depth）×
// 光源逐灯累加，输出 HDR——Lambert 单方向光合成（8.1 形态）在此退役。
//
// **BRDF = Filament 标准模型函数级照抄**（2026-10-22 用户拍板完全照抄口径；
// 对照基准 = shaders/src/surface_brdf.fs 等，本地检出 F:\okzkx\filament
// @ 373190d49，函数清单与常量出处见 .agents/docs/笔记/渲染/Filament 源码参考篇）：
// - D_GGX（Walter 2007，k²/π 形态）
// - V_SmithGGXCorrelated（Heitz 2014 高度相关完整项，desktop QUALITY_HIGH 档）
// - F_Schlick(f0, f90, VoH)，f90 = saturate(16.5·Σf0)（介电体饱和到 1）
// - diffuse = Lambert（BRDF_DIFFUSE 默认宏，Burley 不取）
// - 参数换算链：perceptualRoughness（G-buffer 现值，clamp 0.045 在写入端）→
//   α = perceptual²；computeDiffuseColor = albedo·(1−m)；computeF0 = albedo·m +
//   computeDielectricF0(reflectance)·(1−m)（reflectance 原值从 matparam.a，
//   介电体 F0 = 0.16·reflectance²）；clampNoV 1e-4
// - punctual 衰减：getSquareFalloffAttenuation（smooth 因子平方，falloff =
//   1/radius²）+ 1/max(d²,1e-4)；getAngleAttenuation（spot 角衰减，公式照抄，
//   type 恒 POINT 本段不触发）
// - surfaceShading 组织：NoL 折尾部一次乘（(Fd+Fr)·colorIntensity·intensity·
//   NoL·occlusion），occlusion = AO（matparam.g）；energyCompensation = 1.0
//   （无 IBL DFG LUT，Filament 源内该值即走 1.0 分支形状）
//
// 函数命名保留 Filament 原名（对照时逐名可查）；命名映射：Filament
// shading_view/light.l/h ↔ 本文件 V/L/H，NoV/NoL/NoH/LoH 同名。
//
// 组织形态（与 Filament 的差异，参考篇 §7）：Filament 是 forward 逐片元查
// froxel；本 pass 是 deferred 光照累积——逐像素查 3D cluster（8.1.3 的
// cluster 列表，索引域 = 光源常驻表槽号）再逐灯累加。"逐光源累加到 inout
// color"的循环形状同构。
//
// 模式（frame.mode）：
// - 0 Shading：Filament BRDF 光照累积（Lambert 单光时代 mode 0 的正名——
//   值不变，语义升级）
// - 1 Unlit / 2 Normal / 3 Position：8.1 验收仪器原样保留
// - 4..8 G-buffer 调试仪器（8.2.5）：matparam 单通道灰度直出（metallic/
//   roughness/AO/reflectance——mutate 材质热改的写后读验收口）+
//   reverse-Z 深度线性化灰度
//
// 背景判定：reverse-Z 深度清 0 = 远平面 → 输出清屏色（forward 直出清屏语义
// 的 deferred 承接）。间接光注入项（步骤 9）：本 pass 输出前的 `indirect`
// 累加位已留（ambient 项所在位置），本段只立接口不填内容。
enable wgpu_binding_array;

struct FrameUniforms {
    view_proj: mat4x4f,
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
    // 8.1.2（UBO 128→192B 第二次解冻）：clip→world 反投影矩阵
    inv_view_proj: mat4x4f,
    // 8.2.3（UBO 192→208B 第三次解冻）：相机世界位置（光照累积 V 向量）
    camera_pos: vec4f,
}

// —— cluster 族（与 cluster_build.wgsl/transparent_forward.wgsl 同源互证）

struct LightRecord {
    pos_radius: vec4f,       // xyz 世界位置 + w 半径（米；0 = 死灯，门卫跳过）
    color_intensity: vec4f,  // 颜色（rgb 线性）× 强度（w，candela 域）
    flags: u32,              // bit0 = shadow（8.3 预留）；bits 8..15 = type
    _pad: vec3<u32>,
}

struct ClusterParams {
    view_proj: mat4x4f,
    inv_view_proj: mat4x4f,
    near: f32,
    far: f32,
    tiles_x: u32,
    tiles_y: u32,
    slices: u32,
    light_count: u32,
    extent_w: f32,
    extent_h: f32,
}

struct ClusterRecord {
    count: u32,
    indices: array<u32, 16>,  // MAX_PER_CLUSTER（cluster.rs 同源）
}

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;
@group(1) @binding(5) var matparam_tex: texture_2d<f32>;
@group(2) @binding(0) var<storage, read> cluster_params: ClusterParams;
@group(2) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(2) @binding(2) var<storage, read> clusters: array<ClusterRecord>;
// 8.3 shadow 域（set3）：b0 shadow UBO（级联矩阵/分割带/开关旗标）+
// b1..4 四级联深度图（D32 texture_load 自写滤波——non-filterable 路线，
// 不引 compare sampler）。shadow 关 = shadow_visibility 恒 1（UBO 旗标）。
@group(3) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(3) @binding(1) var shadow_map0: texture_2d<f32>;
@group(3) @binding(2) var shadow_map1: texture_2d<f32>;
@group(3) @binding(3) var shadow_map2: texture_2d<f32>;
@group(3) @binding(4) var shadow_map3: texture_2d<f32>;

struct ShadowUniforms {
    cascade_view_proj: array<mat4x4f, 4>,
    planes: array<vec4f, 24>,
    splits: vec4f,
    camera_forward: vec4f,
    camera_pos: vec4f,
    // x = map_size（PCSS texel 归一）、y = enabled 旗标（0/1）
    map_size: vec4f,
    // 逐级联 texel 世界边长（米）/ 1÷光向盒深（米 → NDC 深度）——bias、
    // 法线抬升、半影都按米算再换算，换级联不失真
    texel_world: vec4f,
    inv_span: vec4f,
}

const MODE_SHADING: u32 = 0u;   // FRAME_MODE_LAMBERT（值不变，语义升级为 BRDF）
const MODE_UNLIT: u32 = 1u;
const MODE_NORMAL: u32 = 2u;
const MODE_POSITION: u32 = 3u;
const MODE_METALLIC: u32 = 4u;      // FRAME_MODE_*（descriptors.rs 同值）
const MODE_ROUGHNESS: u32 = 5u;
const MODE_AO: u32 = 6u;
const MODE_REFLECTANCE: u32 = 7u;
const MODE_DEPTH: u32 = 8u;

// 清屏色（与 host 侧 CLEAR_COLOR 同值——线性域，SRGB 编码在最终 ROP 一次）
const CLEAR_COLOR: vec3f = vec3f(0.02, 0.06, 0.14);
const EXPOSURE: f32 = 0.00016;  // 8.6 重校：完整版 ACES 锚点——直射中灰面 ≈0.25 HDR、白墙 ≈0.85，落 Hill 拟合线性段（肩部压缩留给真高光）；原 0.00032 是 8.4.1 按 Narkowicz 肩部口径校的（白墙 1.7 进肩部），受光面大面积去饱和发灰的主源

// MAX_PER_CLUSTER（cluster.rs 同源）
const MAX_PER_CLUSTER: u32 = 16u;
const CLUSTER_TILE: f32 = 32.0;

// —— Filament 标准模型照抄区（shaders/src/surface_brdf.fs 等，desktop 默认
// 组合；常量与公式逐式对照，命名保留原名）———————————————

const PI: f32 = 3.14159265358979;

fn saturate(v: f32) -> f32 {
    return clamp(v, 0.0, 1.0);
}

fn pow5(v: f32) -> f32 {
    // Filament: pow5(x) = x²·x²·x（两次乘法链）
    let v2 = v * v;
    return v2 * v2 * v;
}

// clampNoV：Neubelt and Pettineo 2013（surface_material.fs MIN_N_DOT_V = 1e-4）
fn clampNoV(NoV: f32) -> f32 {
    return max(NoV, 1e-4);
}

// D_GGX（Walter et al. 2007）——k²/π 重排形态，与教科书 α²/(π(NoH²(α²−1)+1)²)
// 代数等价；453.5 防 fp16 溢出（Filament 原注释），f32 路径保留原式
fn D_GGX(roughness: f32, NoH: f32, h: vec3f) -> f32 {
    let oneMinusNoHSquared = 1.0 - NoH * NoH;
    let a = NoH * roughness;
    let k = min(roughness / (oneMinusNoHSquared + a * a), 453.5);
    return k * (k * (1.0 / PI));
}

// V_SmithGGXCorrelated（Heitz 2014 高度相关完整项；a2 = α²，α 已是 perceptual²）
fn V_SmithGGXCorrelated(roughness: f32, NoV: f32, NoL: f32) -> f32 {
    let a2 = roughness * roughness;
    let lambdaV = NoL * sqrt((NoV - a2 * NoV) * NoV + a2);
    let lambdaL = NoV * sqrt((NoL - a2 * NoL) * NoL + a2);
    return 0.5 / max(lambdaV + lambdaL, 0.0000077);  // PREVENT_DIV0 展开
}

// F_Schlick（双参版）：f90 = saturate(16.5·Σf0) 由调用方传入（介电体饱和到 1，
// 深色金属掠射高光变暗）
fn F_Schlick(f0: vec3f, f90: f32, VoH: f32) -> vec3f {
    return f0 + (f90 - f0) * pow5(1.0 - VoH);
}

// diffuse lobe（BRDF_DIFFUSE 默认 = DIFFUSE_LAMBERT；Burley 可选宏不取）
fn Fd_Lambert() -> f32 {
    return 1.0 / PI;
}

// —— punctual 衰减（surface_light_punctual.fs 照抄）———————————————

// smooth 因子平方 + falloff = 1/radius²（除以平方距离在调用点做）
fn getSquareFalloffAttenuation(distanceSquare: f32, falloff: f32) -> f32 {
    let factor = distanceSquare * falloff;
    let smoothFactor = saturate(1.0 - factor * factor);
    return smoothFactor * smoothFactor;
}

// 距离衰减：Filament 的 lightFarAttenuationParams 远距全局项本工程灯表无此
// 帧常量（其缺省 (1,0) = saturate(1−0) 恒等），省略；1cm 体积防除零照抄
fn getDistanceAttenuation(posToLight: vec3f, falloff: f32) -> f32 {
    let distanceSquare = dot(posToLight, posToLight);
    let attenuation = getSquareFalloffAttenuation(distanceSquare, falloff);
    return attenuation / max(distanceSquare, 1e-4);
}

// 聚光角衰减：内锥全亮、外锥平滑到零，输出平方（spot type 本段不触发，
// 公式照抄为 8.3+ 预留）
fn getAngleAttenuation(lightDir: vec3f, l: vec3f, scaleOffset: vec2f) -> f32 {
    let cd = dot(lightDir, l);
    let attenuation = saturate(cd * scaleOffset.x + scaleOffset.y);
    return attenuation * attenuation;
}

// —— PixelParams 与 surfaceShading（surface_shading_model_standard.fs 照抄）———

struct PixelParams {
    diffuseColor: vec3f,
    f0: vec3f,
    perceptualRoughness: f32,
    roughness: f32,
}

// 参数换算（§2 getStandardPixelParams 同构）：roughness 域换算在 G-buffer 写入端
// 已做（perceptual 域 + clamp 0.045），这里 α = perceptual²、NoV clamp
fn getStandardPixelParams(albedo: vec3f, matparam: vec4f) -> PixelParams {
    let metallic = matparam.r;
    var pixel: PixelParams;
    pixel.perceptualRoughness = matparam.b;
    pixel.roughness = pixel.perceptualRoughness * pixel.perceptualRoughness;
    pixel.diffuseColor = computeDiffuseColor(vec4f(albedo, 1.0), metallic);
    // matparam.a 存的是材质 reflectance 原值，必须先换成介电体 F0 再进
    // computeF0——直接传原值 0.5 会让 F0 变成 0.5（应为 0.04），受光面整片
    // 叠一层白色高光、颜色发灰
    let reflectance = computeDielectricF0(matparam.a);
    pixel.f0 = computeF0(vec4f(albedo, 1.0), metallic, reflectance);
    return pixel;
}

// baseColor 参数形状照 Filament（vec4，a 不消费）
fn computeDiffuseColor(baseColor: vec4f, metallic: f32) -> vec3f {
    return baseColor.rgb * (1.0 - metallic);
}

// reflectance 原值 → 介电体 F0（默认 0.5 → 0.04，空气到 IOR 1.5 界面）
fn computeDielectricF0(reflectance: f32) -> f32 {
    return 0.16 * reflectance * reflectance;
}

// reflectance 参数是已换算的介电体 F0（见 computeDielectricF0）
fn computeF0(baseColor: vec4f, metallic: f32, reflectance: f32) -> vec3f {
    return baseColor.rgb * metallic + (reflectance * (1.0 - metallic));
}

// surfaceShading（Filament 原组织）：NoL 折尾部一次乘，diffuse/specular 共享
// 同一个 NoL；occlusion（AO）乘在光源贡献上；energyCompensation = 1.0（无 IBL）
fn surfaceShading(pixel: PixelParams, light_l: vec3f, light_colorIntensity: vec4f, light_attenuation: f32, light_NoL: f32, V: vec3f, n: vec3f, NoV: f32, occlusion: f32) -> vec3f {
    let h = normalize(V + light_l);
    let NoL = light_NoL;                  // 光源侧已 saturate
    let NoH = saturate(dot(n, h));
    let LoH = saturate(dot(light_l, h));

    // specular lobe（isotropicLobe 标准模型形状，energyCompensation = 1.0）
    let roughness = pixel.roughness;
    let D = D_GGX(roughness, NoH, h);
    let Vterm = V_SmithGGXCorrelated(roughness, NoV, NoL);
    // f90 = saturate(50.0 * 0.33 * Σf0)（非 SPECULAR_COLOR_FACTOR 路径原式）
    let f90 = saturate(dot(pixel.f0, vec3f(50.0 * 0.33)));
    let F = F_Schlick(pixel.f0, f90, LoH);
    let Fr = (D * Vterm) * F;

    // diffuse lobe
    let Fd = pixel.diffuseColor * Fd_Lambert();

    let color = Fd + Fr;
    return (color * light_colorIntensity.rgb)
        * (light_colorIntensity.w * light_attenuation * NoL * occlusion);
}

// —— 全屏三角形与 G-buffer 读数（8.1 骨架原样）—————————————————

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // 全屏三角形（无需顶点 buffer）：三个越界角点覆盖视口。
    // uv.y 取 0.5 - p.y/2：naga 对 @builtin(position) 注入 Y 翻转（正高度
    // viewport 朝向定案的同一机制），翻转发生在 clip 写出、uv 不随之翻——
    // 图像坐标 y 向下，顶行 ↔ shader-NDC y=+1 ↔ uv.y=0。
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: VsOut;
    out.clip = vec4f(p[vi], 0.0, 1.0);
    out.uv = vec2f(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return out;
}

// —— 8.3 PCSS 软阴影（textureLoad 路线）：
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
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
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
    // —— G-buffer 调试仪器（8.2.5）：matparam 单通道灰度直出。8bit 量化面
    // 如实呈现（写入端 clamp/域换算一并可见）；背景已在上方清屏色早退。
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
        // reverse-Z 深度线性化。注意片段阶段 @builtin(position).w = 1/clip.w
        //（与 cluster tile 用 in.clip.xy 当像素坐标同一套语义），不能直读当
        // 视深——走反投影重建世界位置，再经 view_proj 取 clip.w = 视空间深度
        //（透视 w = 正距离），除 far 归一。far 取 cluster 参数面（同帧同相机）。
        let world = world_from_depth(in.uv, depth.r);
        let view_depth = (frame.view_proj * vec4f(world, 1.0)).w;
        let t = saturate(view_depth / max(cluster_params.far, 1e-4));
        return vec4f(vec3f(t), 1.0);
    }

    // —— Shading（mode 0 及未知值兜底）：Filament BRDF 光照累积
    let matparam = textureSampleLevel(matparam_tex, gbuf_sampler, in.uv, 0.0);
    let world_pos = world_from_depth(in.uv, depth.r);
    let V = normalize(frame.camera_pos.xyz - world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    let pixel = getStandardPixelParams(albedo.rgb, matparam);
    // AO（matparam.g）乘光源贡献尾部（参考篇 §4 occlusion 语义）；环境 AO
    //（multiBounceAO）归步骤 9 间接光
    let occlusion = matparam.g;

    // 逐灯累加（光源常驻表 × 3D cluster 列表；early exit 同 Filament：
    // NoL ≤ 0 || attenuation ≤ 0 不积累颜色）
    var light_sum = vec3f(0.0);
    if cluster_params.light_count > 0u {
        let tile = vec2<u32>(in.clip.xy / vec2f(CLUSTER_TILE));
        let tile_i = min(tile.x, cluster_params.tiles_x - 1u)
            + min(tile.y, cluster_params.tiles_y - 1u) * cluster_params.tiles_x;
        // 视图深度（clip.w）→ 指数分片（与 cluster_build/CPU 参照同式）
        let ratio = cluster_params.far / cluster_params.near;
        let z = clamp(in.clip.w, cluster_params.near, cluster_params.far);
        let slice = min(u32(f32(cluster_params.slices) * log2(z / cluster_params.near) / log2(ratio)), cluster_params.slices - 1u);
        let ci = tile_i + slice * cluster_params.tiles_x * cluster_params.tiles_y;
        let record = clusters[ci];
        for (var k = 0u; k < record.count; k = k + 1u) {
            let li = record.indices[k];
            let light = lights[li];
            let posToLight = light.pos_radius.xyz - world_pos;
            let l = normalize(posToLight);
            let NoL = saturate(dot(n, l));
            let falloff = 1.0 / max(light.pos_radius.w * light.pos_radius.w, 1e-8);
            let attenuation = getDistanceAttenuation(posToLight, falloff);
            if (NoL > 0.0 && attenuation > 0.0) {
                light_sum = light_sum + surfaceShading(
                    pixel, l, light.color_intensity, attenuation, NoL, V, n, NoV, occlusion);
            }
        }
    }

    // —— 方向光（set1 UBO 单光，8.2.2 钉明不进表）：Filament 方向光形状 =
    // NoL × colorIntensity，无 attenuation；L 取 UBO 单位向量（零向量兜底与
    // forward 时代同式）。indirect 注入位（步骤 9）：间接项累加在 ambient 旁。
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        // 8.3 PCSS：方向光项乘阴影可见性（透明层/环境项不接影——段边界）
        let shadow = shadow_visibility(world_pos, n);
        // 强度已折进 rgb（bevy 同款 illuminance 预乘，w 传 1.0——Filament 的
        // colorIntensity.w 预曝光位由 rgb 内含强度承担）
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, occlusion)
            * shadow;
    }

    // 环境光项保持在光源循环外（参考篇 §6.4）；albedo × ambient = 现役
    // Lambert 合成的同款语义
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    // 间接光注入项（步骤 9）：空接口，本段不填
    let indirect = vec3f(0.0);
    return vec4f((light_sum + ambient + indirect) * EXPOSURE, 1.0);
}
