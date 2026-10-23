// Filament 标准模型 BRDF（函数级照抄，命名保留 Filament 原名以便逐名对照；
// 对照基准 = filament shaders/src/surface_brdf.fs、surface_light_punctual.fs、
// surface_shading_model_standard.fs，本地检出 F:\okzkx\filament @ 373190d49，
// 函数清单与常量出处见 .agents/docs/笔记/渲染/Filament 源码参考篇）。
// 不透明光照累积（gbuffer_composite）与透明前向（transparent_forward）共用。
// - D_GGX（Walter 2007，k²/π 形态）
// - V_SmithGGXCorrelated（Heitz 2014 高度相关完整项，desktop QUALITY_HIGH 档）
// - F_Schlick(f0, f90, VoH)，f90 = saturate(16.5·Σf0)（介电体饱和到 1）
// - diffuse = Lambert（BRDF_DIFFUSE 默认宏，Burley 不取）
// - 参数换算链：α = perceptualRoughness²；computeDiffuseColor = albedo·(1−m)；
//   computeF0 = albedo·m + computeDielectricF0(reflectance)·(1−m)（介电体
//   F0 = 0.16·reflectance²）；clampNoV 1e-4
// - punctual 衰减：getSquareFalloffAttenuation（smooth 因子平方，falloff =
//   1/radius²）+ 1/max(d²,1e-4)
// - surfaceShading：NoL 折尾部一次乘（(Fd+Fr)·colorIntensity·intensity·NoL·
//   occlusion）；energyCompensation = 1.0（无 IBL DFG LUT）
// 命名映射：Filament shading_view / light.l / h ↔ 本文件 V / L / H。

const PI: f32 = 3.14159265358979;
// 曝光锚点（完整版 ACES 口径）：直射中灰面 ≈0.25 HDR、白墙 ≈0.85，落 Hill 拟合
// 线性段，肩部压缩留给真高光。坑点：按 Narkowicz 肩部口径校的 0.00032 会让白墙
// 进肩部，受光面大面积去饱和发灰。不透明与透明层必须同值。
const EXPOSURE: f32 = 0.00016;

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

// F_Schlick（双参版）：f90 由调用方传入（介电体饱和到 1，深色金属掠射高光变暗）
fn F_Schlick(f0: vec3f, f90: f32, VoH: f32) -> vec3f {
    return f0 + (f90 - f0) * pow5(1.0 - VoH);
}

// diffuse lobe（BRDF_DIFFUSE 默认 = DIFFUSE_LAMBERT）
fn Fd_Lambert() -> f32 {
    return 1.0 / PI;
}

// smooth 因子平方 + falloff = 1/radius²（除以平方距离在调用点做）
fn getSquareFalloffAttenuation(distanceSquare: f32, falloff: f32) -> f32 {
    let factor = distanceSquare * falloff;
    let smoothFactor = saturate(1.0 - factor * factor);
    return smoothFactor * smoothFactor;
}

// 距离衰减：Filament 的 lightFarAttenuationParams 远距全局项本工程无此帧常量
//（其缺省 (1,0) = saturate(1−0) 恒等），省略；1cm 体积防除零照抄
fn getDistanceAttenuation(posToLight: vec3f, falloff: f32) -> f32 {
    let distanceSquare = dot(posToLight, posToLight);
    let attenuation = getSquareFalloffAttenuation(distanceSquare, falloff);
    return attenuation / max(distanceSquare, 1e-4);
}

// 聚光角衰减：内锥全亮、外锥平滑到零，输出平方（灯表 type 恒 POINT，尚无调用方）
fn getAngleAttenuation(lightDir: vec3f, l: vec3f, scaleOffset: vec2f) -> f32 {
    let cd = dot(lightDir, l);
    let attenuation = saturate(cd * scaleOffset.x + scaleOffset.y);
    return attenuation * attenuation;
}

struct PixelParams {
    diffuseColor: vec3f,
    f0: vec3f,
    roughness: f32,
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

// getStandardPixelParams 同构：perceptualRoughness 已在材质换算端 clamp 0.045。
// 坑点：reflectance 是材质原值，必须先经 computeDielectricF0 换成 F0——直接传
// 0.5 会让 F0 = 0.5（应为 0.04），受光面整片叠白色高光、颜色发灰。
fn getStandardPixelParams(albedo: vec3f, metallic: f32, perceptualRoughness: f32, reflectance: f32) -> PixelParams {
    var pixel: PixelParams;
    pixel.roughness = perceptualRoughness * perceptualRoughness;
    pixel.diffuseColor = computeDiffuseColor(vec4f(albedo, 1.0), metallic);
    pixel.f0 = computeF0(vec4f(albedo, 1.0), metallic, computeDielectricF0(reflectance));
    return pixel;
}

// surfaceShading（Filament 原组织）：diffuse/specular 共享同一个 NoL（光源侧已
// saturate）；occlusion（AO）乘在光源贡献上；energyCompensation = 1.0
fn surfaceShading(pixel: PixelParams, light_l: vec3f, light_colorIntensity: vec4f, light_attenuation: f32, light_NoL: f32, V: vec3f, n: vec3f, NoV: f32, occlusion: f32) -> vec3f {
    let h = normalize(V + light_l);
    let NoL = light_NoL;
    let NoH = saturate(dot(n, h));
    let LoH = saturate(dot(light_l, h));

    // specular lobe（isotropicLobe 标准模型形状）
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

// 方向光 L：UBO dir_to_light 归一化，零向量兜底朝上。
fn safe_light_dir(l_raw: vec3f) -> vec3f {
    return select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
}
