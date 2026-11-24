// 延迟光照累积（全屏）：读 G-buffer（albedo/normal/matparam/emissive/depth），逐像素查
// 3D cluster 列表（索引 = 光源常驻表槽号）逐灯累加 BRDF（common/brdf.wgsl），叠加
// 方向光（乘 PCSS 阴影可见性）、环境光与自发光，输出 HDR。
//
// 模式（frame.mode，FRAME_MODE_*）：
// - 0 Shading：BRDF 光照累积（未知值同样走此分支）
// - 1 Unlit / 2 Normal / 3 Position：反照率 / 世界法线 / 深度重建世界位置直出
// - 4..8 调试显示：matparam 单通道灰度直出（metallic/roughness/AO/reflectance）
//   + reverse-Z 深度线性化灰度
// - 9/10 IBL 漫反射/高光贡献直出（IBL 关时黑）
// - 11 GlobalProbe 桶可视化：法线选桶 → 桶中心方向着色（9.3 的映射取证）
//
// 环境项：IBL 开 = 漫反射（SH-L2 或 GlobalProbe 16 桶，按 frame.ibl_mode）+ split-sum
// 高光（**替换**旧常量项，不叠加）；IBL 关 = 步骤 8 原文 `albedo · ambient_color`，
// 逐位一致（回归硬线）。
//
// 背景判定：reverse-Z 深度清 0 = 远平面 → 程序化天空（sky_enabled 关时回清屏色）。
enable wgpu_binding_array;
// #import frame
// #import cluster_lighting
// #import shadow_ubo
// #import fullscreen
// #import sky
// #import ibl

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;
@group(1) @binding(5) var matparam_tex: texture_2d<f32>;
@group(1) @binding(6) var emissive_tex: texture_2d<f32>;
@group(2) @binding(0) var<storage, read> cluster_params: ClusterParams;
@group(2) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(2) @binding(2) var<storage, read> clusters: array<ClusterRecord>;
// set3 阴影：b0 阴影 UBO（级联矩阵/分割带/开关）+ b1..4 四级联深度图（D32 不可
// 过滤，用 textureLoad 自写滤波，不用 compare sampler）。阴影关时可见性恒 1。
@group(3) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(3) @binding(1) var shadow_map0: texture_2d<f32>;
@group(3) @binding(2) var shadow_map1: texture_2d<f32>;
@group(3) @binding(3) var shadow_map2: texture_2d<f32>;
@group(3) @binding(4) var shadow_map3: texture_2d<f32>;
// set4 环境光照：b0 预滤波 cube（全 mip）、b1 采样器（cube 与 LUT 共用）、b2 DFG LUT、
// b3 SH-L2 系数（9×vec4，已折叠余弦卷积的 A_l）、b4 GlobalProbe 16 桶（16×4 vec4，
// 已折叠基函数常数；求值式只剩 l0 + l1·n）
@group(4) @binding(0) var env_cube: texture_cube<f32>;
@group(4) @binding(1) var ibl_sampler: sampler;
@group(4) @binding(2) var dfg_lut: texture_2d<f32>;
@group(4) @binding(3) var<storage, read> sh_coeffs: array<vec4f, 9>;
@group(4) @binding(4) var<storage, read> probe_buckets: array<vec4f, 64>;

const MODE_SHADING: u32 = 0u;   // 宿主侧名为 FRAME_MODE_LAMBERT
const MODE_UNLIT: u32 = 1u;
const MODE_NORMAL: u32 = 2u;
const MODE_POSITION: u32 = 3u;
const MODE_METALLIC: u32 = 4u;
const MODE_ROUGHNESS: u32 = 5u;
const MODE_AO: u32 = 6u;
const MODE_REFLECTANCE: u32 = 7u;
const MODE_DEPTH: u32 = 8u;
const MODE_IBL_DIFFUSE: u32 = 9u;
const MODE_IBL_SPECULAR: u32 = 10u;
const MODE_PROBE_BUCKETS: u32 = 11u;

// 清屏色（线性域），须与宿主侧 CLEAR_COLOR 同值
const CLEAR_COLOR: vec3f = vec3f(0.02, 0.06, 0.14);

// —— PCSS 软阴影（textureLoad 路线）：
// 按视深选级联；法线抬升后的投影点落出本级盒（含采样核余量）就退到更粗
// 一级，四级都出盒才判无影。深度比较 reverse-Z：更近太阳 = 值更大。
// bias / 法线抬升 / 半影都按米计算（texel_world、inv_span 换算），换级联
// 世界尺度一致。UBO map_size.y < 0.5 表示阴影关闭。
// 采样核是逐像素旋转的螺旋盘（不是规则网格）：近级联 texel 世界尺寸约 1cm，
// 屏幕上一个 texel 不足一像素，固定网格会把阴影图 texel 网格叠成可见交叉网纹。
// 级间按视深在分割带前一段带宽内混合相邻两级（见 shadow_visibility）。

// 半影宽度 / 遮挡体到接收面距离（米/米），约等于把太阳当成有面积的光源
const SHADOW_PENUMBRA_SCALE: f32 = 0.02;
// 半影半径上限（texel）；同时是出盒判定的采样核余量
const SHADOW_MAX_RADIUS: f32 = 4.0;
// 采样盘 tap 数（黄金角螺旋，半径按 sqrt((i+0.5)/N) 分布 → 盘内面积均匀）
const SHADOW_TAPS: u32 = 16u;
// 黄金角（弧度）：相邻 tap 的角步长，使螺旋在盘内无可见径向条带
const SHADOW_GOLDEN_ANGLE: f32 = 2.3999632;
// blocker 搜索盘半径（texel）：与原 3×3 步长 2（覆盖 ±2 texel）同域
const SHADOW_BLOCKER_RADIUS: f32 = 2.0;
// 级联淡出带宽占本级覆盖范围的比例：末段与下一级线性混合，消除分割带硬切
const SHADOW_FADE_FRACTION: f32 = 0.10;
// 级联覆盖近端（米），镜像 cascades.rs 的 SHADOW_NEAR_DISTANCE
const SHADOW_NEAR_DISTANCE: f32 = 0.5;

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

// 逐像素旋转相位（弧度）：哈希钉在世界坐标上——相机移动时采样相位不随屏幕漂移
// （无 TAA，逐像素抖动若随屏幕变化会让半影像素"沸腾"）；相邻像素世界点相差 ~1cm
// 就完全去相关，足够把规则网格打散成高频噪点。
fn shadow_rotation(world_pos: vec3f) -> f32 {
    let p = world_pos.xz * 733.0 + vec2f(world_pos.y * 317.0, world_pos.y * 419.0);
    return fract(sin(dot(p, vec2f(12.9898, 78.233))) * 43758.5453) * 6.2831853;
}

// 单级联 PCSS：px = 接收点 texel 坐标，own = 接收点 reverse-Z 光深度。
// 斜面上采样点离中心越远，表面自身在图里越"高"，bias 按 tap 距离 × 坡度加。
fn shadow_pcss(ci: u32, px: vec2f, own: f32, no_l: f32, rot: f32) -> f32 {
    let texel_world = shadow.texel_world[ci];
    let inv_span = shadow.inv_span[ci];
    let tan_t = sqrt(max(1.0 - no_l * no_l, 0.0)) / max(no_l, 0.25);
    let bias_m = texel_world * (1.0 + tan_t) + 0.01;
    let slope_m = texel_world * tan_t;
    let center = vec2<i32>(px);
    // 旋转盘：黄金角步进（复数旋转）代替逐 tap 三角函数；blocker 与 PCF 同一相位。
    let step_c = cos(SHADOW_GOLDEN_ANGLE);
    let step_s = sin(SHADOW_GOLDEN_ANGLE);
    let rot_c = cos(rot);
    let rot_s = sin(rot);
    // —— blocker search（16 tap 旋转盘，外圈 2 texel）
    var blocker_sum = 0.0;
    var blocker_count = 0.0;
    var rc = rot_c;
    var rs = rot_s;
    for (var i = 0u; i < SHADOW_TAPS; i = i + 1u) {
        let r = sqrt((f32(i) + 0.5) * (1.0 / f32(SHADOW_TAPS))) * SHADOW_BLOCKER_RADIUS;
        let off = vec2f(rc, rs) * r;
        let d = shadow_map_load(ci, center + vec2<i32>(round(off)));
        let tap_bias = (bias_m + slope_m * length(off)) * inv_span;
        if (d > own + tap_bias) {
            blocker_sum = blocker_sum + d;
            blocker_count = blocker_count + 1.0;
        }
        let next_c = rc * step_c - rs * step_s;
        rs = rc * step_s + rs * step_c;
        rc = next_c;
    }
    if (blocker_count == 0.0) {
        return 1.0;
    }
    // —— 半影半径：遮挡体到接收面的米距离 × 光源尺度 → texel
    let blocker_m = (blocker_sum / blocker_count - own) / inv_span;
    let radius = clamp(blocker_m * SHADOW_PENUMBRA_SCALE / texel_world, 1.0, SHADOW_MAX_RADIUS);
    // —— PCF（16 tap 旋转盘，外圈 = 半影半径）
    var lit = 0.0;
    rc = rot_c;
    rs = rot_s;
    for (var i = 0u; i < SHADOW_TAPS; i = i + 1u) {
        let r = sqrt((f32(i) + 0.5) * (1.0 / f32(SHADOW_TAPS))) * radius;
        let off = vec2f(rc, rs) * r;
        let d = shadow_map_load(ci, center + vec2<i32>(round(off)));
        let tap_bias = (bias_m + slope_m * length(off)) * inv_span;
        lit = lit + select(1.0, 0.0, d > own + tap_bias);
        let next_c = rc * step_c - rs * step_s;
        rs = rc * step_s + rs * step_c;
        rc = next_c;
    }
    return lit / f32(SHADOW_TAPS);
}

// 单级采样：投影点（法线抬升后）落出本级盒返回 -1——调用方按原语义退到下一级。
fn shadow_cascade(ci: u32, world_pos: vec3f, n: vec3f, no_l: f32, rot: f32) -> f32 {
    let size = shadow.map_size.x;
    let margin = SHADOW_MAX_RADIUS + 3.0;
    // 法线抬升（防自遮蔽）：1~3 texel，掠射面抬得多
    let lifted = world_pos + n * (shadow.texel_world[ci] * (1.0 + 2.0 * (1.0 - no_l)));
    let cs = shadow.cascade_view_proj[ci] * vec4f(lifted, 1.0);
    let ndc = cs.xyz / cs.w;
    let px = vec2f(ndc.x * 0.5 + 0.5, 0.5 - 0.5 * ndc.y) * size;
    let inside = all(px >= vec2f(margin)) && all(px <= vec2f(size - margin))
        && ndc.z >= 0.0 && ndc.z <= 1.0;
    if (!inside) {
        return -1.0;
    }
    return shadow_pcss(ci, px, ndc.z, no_l, rot);
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
    let rot = shadow_rotation(world_pos);
    // 选级 + 出盒退级：本级不可采样就试更粗一级，四级都出盒才判无影
    var used = 4u;
    var vis = 1.0;
    for (var c = start; c < 4u; c = c + 1u) {
        let v = shadow_cascade(c, world_pos, n, no_l, rot);
        if (v >= 0.0) {
            vis = v;
            used = c;
            break;
        }
    }
    if (used >= 4u) {
        return 1.0;
    }
    // 级联淡出：只在本级覆盖范围末段（10% 带宽）与下一级混合。视深在淡出带内时
    // 两级各采一次，smoothstep 权重从 0 到 1；投影点在本级盒内才走到这里，出盒
    // fallback（used > start）的视深不落在本级末段，天然不参与淡出。
    if (used + 1u < 4u) {
        let prev = max(used, 1u) - 1u;
        let near = select(SHADOW_NEAR_DISTANCE, shadow.splits[prev], used > 0u);
        let far = shadow.splits[used];
        let band = max((far - near) * SHADOW_FADE_FRACTION, 1e-3);
        let fade = smoothstep(far - band, far, view_depth);
        if (fade > 0.0) {
            let next = shadow_cascade(used + 1u, world_pos, n, no_l, rot);
            if (next >= 0.0) {
                vis = mix(vis, next, fade);
            }
        }
    }
    return vis;
}

// 深度反投影：uv → shader-NDC（uv.y = 0.5 - 0.5·ndc.y 的逆变换）→ clip → world
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

// —— 环境光照（IBL）：漫反射 SH-L2 辐照度 + 高光 split-sum。IBL 关时两项为 0
// （调用方用 select 回旧常量项，不在这里混合）。
struct IblTerms {
    diffuse: vec3f,
    specular: vec3f,
}

fn ibl_environmental(
    pixel: PixelParams,
    n_in: vec3f,
    v: vec3f,
    perceptual_roughness: f32,
) -> IblTerms {
    var terms: IblTerms;
    if (frame.ibl_enabled == 0u) {
        return terms;
    }
    // 9.2.8：入射法线翻到视向侧（双面几何背面）+ specular AA（法线导数撑开粗糙度）。
    // 两个辅助函数在 common/ibl.wgsl，透明层同口径。
    let n = ibl_normal(n_in, v);
    let no_v = clampNoV(saturate(dot(n, v)));
    let p_ibl = ibl_roughness(n, perceptual_roughness);
    // 漫反射编码二选一（9.3）：0 = SH-L2、1 = GlobalProbe 16 桶。两条都吃同一个
    // 翻转后的法线（9.2.8 口径），否则双面几何上 A/B 比的是两条法线。
    var irradiance = sh_irradiance(n, sh_coeffs);
    if (frame.ibl_mode == 1u) {
        let base = direction_to_bucket_index(n, 4u) * 4u;
        irradiance = probe16_eval(
            n,
            probe_buckets[base].xyz,
            probe_buckets[base + 1u].xyz,
            probe_buckets[base + 2u].xyz,
            probe_buckets[base + 3u].xyz);
    }
    // E(n) 已含余弦卷积与基函数常数，Lambert BRDF 只再乘 1/π
    terms.diffuse = pixel.diffuseColor * Fd_Lambert() * irradiance
        * frame.ibl_intensity;
    // 坑：粗糙度必须用 perceptualRoughness（matparam.b），pixel.roughness 是 α = p²，
    // 直接喂 mip 会把粗糙面算得过锐
    terms.specular = ibl_specular(
        env_cube, ibl_sampler, dfg_lut, n, v, no_v, pixel.f0, p_ibl, IBL_MAX_MIP)
        * frame.ibl_intensity;
    return terms;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let albedo = textureSampleLevel(albedo_tex, gbuf_sampler, in.uv, 0.0);
    let depth = textureSampleLevel(depth_tex, gbuf_sampler, in.uv, 0.0);
    // 背景：reverse-Z 清 0 = 远平面，无几何写深度
    if (depth.r <= 1e-6) {
        // 天空只在 Shading 画：调试视图（mode 1..8）保持清屏色，免得仪器视图被污染。
        // 关天空时原样回清屏色（不乘曝光、不做等价变形），与无天空基线逐位一致。
        if (frame.sky_enabled == 0u || frame.mode != MODE_SHADING) {
            return vec4f(CLEAR_COLOR, 1.0);
        }
        // 方向 = 近平面点 − 相机位置（透视下就是该像素的射线方向）。坑：不能用
        // ndc.z=0（远平面）反投影——reverse-Z + 无限远投影下那是 w=0 的无穷远点，
        // 归一化出 NaN，天空会退化成全屏地平线色。ndc.z=1 是近平面，反投影有限。
        let v = world_from_depth(in.uv, 1.0) - frame.camera_pos.xyz;
        let dir = select(vec3f(0.0, 1.0, 0.0), normalize(v), dot(v, v) > 1e-12);
        // 天空与直接光同在 pre-exposure 域：这里乘曝光，天空函数内部不乘
        return vec4f(sky_radiance(dir, true) * EXPOSURE, 1.0);
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
    // —— 调试显示：matparam 单通道灰度直出（8bit 量化后的原值）
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
        // reverse-Z 深度线性化。坑：片段阶段 @builtin(position).w = 1/clip.w，不能直接
        // 当视深——先反投影重建世界位置，再经 view_proj 取 clip.w（透视下即正的视深），
        // 除以 far 归一（far 取 cluster 参数，同帧同相机）。
        let world = world_from_depth(in.uv, depth.r);
        let view_depth = (frame.view_proj * vec4f(world, 1.0)).w;
        let t = saturate(view_depth / max(cluster_params.far, 1e-4));
        return vec4f(vec3f(t), 1.0);
    }

    // —— Shading（mode 0 及未知值）：BRDF 光照累积
    // matparam：r = metallic / g = AO / b = perceptualRoughness / a = reflectance 原值
    let matparam = textureSampleLevel(matparam_tex, gbuf_sampler, in.uv, 0.0);
    let world_pos = world_from_depth(in.uv, depth.r);
    let V = normalize(frame.camera_pos.xyz - world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    let pixel = getStandardPixelParams(albedo.rgb, matparam.r, matparam.b, matparam.a);
    // AO 乘在直接光贡献上
    let occlusion = matparam.g;

    // 点光源先累加、方向光后加（累加顺序即浮点结果，保持不变）
    var light_sum = accumulate_cluster_lights(
        vec3f(0.0), cluster_params, in.clip.xy, pixel, world_pos, V, n, NoV, occlusion);

    // —— 方向光（帧 UBO 里的单光，不进灯表）：NoL × 颜色强度，无衰减
    let l = safe_light_dir(frame.dir_to_light.xyz);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        // 只有方向光乘阴影可见性（透明层/环境项不接影）
        let shadow = shadow_visibility(world_pos, n);
        // 强度已预乘进 rgb，w 传 1.0
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, occlusion)
            * shadow;
    }

    // 环境项：IBL 开 = 漫反射 + 高光（替换旧常量项）；关 = 步骤 8 原文 albedo × ambient。
    // 两条调试档（9/10）直出 IBL 单项贡献，用来读方向性与能量。
    let ibl = ibl_environmental(pixel, n, V, matparam.b);
    if frame.mode == MODE_IBL_DIFFUSE {
        return vec4f(ibl.diffuse * EXPOSURE, 1.0);
    }
    if frame.mode == MODE_IBL_SPECULAR {
        return vec4f(ibl.specular * EXPOSURE, 1.0);
    }
    if frame.mode == MODE_PROBE_BUCKETS {
        // 桶可视化（9.3）：法线选桶 → 桶中心方向 RGB 编码，用来肉眼核对映射/接缝
        let center = bucket_index_to_direction(direction_to_bucket_index(n, 4u), 4u);
        return vec4f(center * 0.5 + vec3f(0.5), 1.0);
    }
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    let environment = select(ambient, ibl.diffuse + ibl.specular, frame.ibl_enabled != 0u);
    // 自发光：不受光照/阴影/AO 影响，直接加进 HDR。Bevy StandardMaterial 默认
    // emissive_exposure_weight = 0，即自发光不随相机曝光缩放；本引擎的 EXPOSURE
    // 锚在 20000 lx 直射（0.00016），跟着乘会把 glTF 的 emissiveFactor=1 压到
    // 不可见。加在曝光乘算之外，结果同样写进 HDR、跟着进 Bloom。
    let emissive = textureSampleLevel(emissive_tex, gbuf_sampler, in.uv, 0.0).rgb;
    return vec4f((light_sum + environment) * EXPOSURE + emissive, 1.0);
}
