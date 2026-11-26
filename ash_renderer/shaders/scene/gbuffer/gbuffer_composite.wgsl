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
// #import shadow
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
// 9.7.6 SCGI：全分辨率辐照度（9.7.5 的上采样产物）。按像素 `textureLoad`，不走采样器——
// 上采样已经做完，这里只要"这一像素的那一项"。`a` = 覆盖率（0 = 不接管，回 IBL 基线）。
@group(5) @binding(0) var scgi_irradiance: texture_2d<f32>;

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

// CSM 阴影采样在 common/shadow.wgsl（9.6.2 抽取，合成端与 estimation compute 共用）：
// 选级联、法线抬升投影、出盒退级、级间淡出、PCSS 与低档 5 tap PCF 都在那一份里；
// 本入口只声明 shadow UBO 与四张级联图（见下方 set3），调用时把光方向传进去。


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
        let shadow = shadow_visibility(world_pos, n, l);
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
    // 9.7.6 SCGI 注入：只换 IBL 的**漫反射**项，高光原样；`a == 0`（Volume 外 / 无命中 /
    // SCGI 关）逐像素回 IBL 基线——`mix` 两边都已算好，不存在"把 IBL 一侧写成 0 再混"的整屏变暗。
    // 缓存 E 已是绝对辐照度：天空入口已乘 ibl_intensity，直接光与自发光弹射也不能再被它缩放。
    var scgi_diffuse = ibl.diffuse;
    if (frame.scgi.x != 0u && frame.ibl_enabled != 0u) {
        let irr = textureLoad(
            scgi_irradiance,
            vec2i(i32(in.clip.x), i32(in.clip.y)),
            0,
        );
        let replacement = pixel.diffuseColor * Fd_Lambert() * irr.rgb;
        scgi_diffuse = mix(ibl.diffuse, replacement, clamp(irr.a, 0.0, 1.0));
    }
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    let environment = select(
        ambient,
        scgi_diffuse + ibl.specular,
        frame.ibl_enabled != 0u,
    );
    // 自发光：不受光照/阴影/AO 影响，直接加进 HDR。Bevy StandardMaterial 默认
    // emissive_exposure_weight = 0，即自发光不随相机曝光缩放；本引擎的 EXPOSURE
    // 锚在 20000 lx 直射（0.00016），跟着乘会把 glTF 的 emissiveFactor=1 压到
    // 不可见。加在曝光乘算之外，结果同样写进 HDR、跟着进 Bloom。
    let emissive = textureSampleLevel(emissive_tex, gbuf_sampler, in.uv, 0.0).rgb;
    return vec4f((light_sum + environment) * EXPOSURE + emissive, 1.0);
}
