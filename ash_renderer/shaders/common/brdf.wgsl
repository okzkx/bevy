// 标准模型 BRDF。函数名沿用 Filament 原名以便逐名对照；函数清单与常量出处见
// .agents/docs/笔记/渲染/Filament 源码参考：标准模型 BRDF 与多光源（8.2 对照基准）.md。
// 不透明光照累积（gbuffer_composite）与透明前向（transparent_forward）共用。
// - 镜面：D_GGX × V_SmithGGXCorrelated × F_Schlick，f90 = saturate(16.5·Σf0)
// - 漫反射：Lambert
// - α = perceptualRoughness²；diffuse = albedo·(1−m)；F0 = albedo·m + 介电体 F0·(1−m)
// - 点光衰减：平滑窗口（falloff = 1/radius²）× 1/max(d², 1e-4)
// - 无 IBL，energyCompensation = 1
// 命名对应：Filament 的 shading_view / light.l / h ↔ 本文件 V / L / H。

const PI: f32 = 3.14159265358979;
// 曝光锚点（完整 ACES 曲线）：直射中灰面 ≈0.25 HDR、白墙 ≈0.85，落在曲线线性段，
// 肩部压缩留给真高光。坑：按 Narkowicz 肩部校的 0.00032 会让白墙进肩部，受光面
// 大面积去饱和发灰。不透明与透明层必须同值。
const EXPOSURE: f32 = 0.00016;

fn saturate(v: f32) -> f32 {
    return clamp(v, 0.0, 1.0);
}

fn pow5(v: f32) -> f32 {
    let v2 = v * v;
    return v2 * v2 * v;
}

// NoV 下限 1e-4，防掠射角数值问题
fn clampNoV(NoV: f32) -> f32 {
    return max(NoV, 1e-4);
}

// D_GGX：教科书 α²/(π(NoH²(α²−1)+1)²) 的重排形态；453.5 上限原为防 fp16 溢出，f32 下保留
fn D_GGX(roughness: f32, NoH: f32, h: vec3f) -> f32 {
    let oneMinusNoHSquared = 1.0 - NoH * NoH;
    let a = NoH * roughness;
    let k = min(roughness / (oneMinusNoHSquared + a * a), 453.5);
    return k * (k * (1.0 / PI));
}

// V_SmithGGXCorrelated（高度相关完整项；a2 = α²，α 已是 perceptual²）
fn V_SmithGGXCorrelated(roughness: f32, NoV: f32, NoL: f32) -> f32 {
    let a2 = roughness * roughness;
    let lambdaV = NoL * sqrt((NoV - a2 * NoV) * NoV + a2);
    let lambdaL = NoV * sqrt((NoL - a2 * NoL) * NoL + a2);
    return 0.5 / max(lambdaV + lambdaL, 0.0000077);  // 防除零
}

// F_Schlick（双参版）：f90 由调用方传入
fn F_Schlick(f0: vec3f, f90: f32, VoH: f32) -> vec3f {
    return f0 + (f90 - f0) * pow5(1.0 - VoH);
}

fn Fd_Lambert() -> f32 {
    return 1.0 / PI;
}

// 平滑窗口的平方，falloff = 1/radius²（除以平方距离在调用点做）
fn getSquareFalloffAttenuation(distanceSquare: f32, falloff: f32) -> f32 {
    let factor = distanceSquare * falloff;
    let smoothFactor = saturate(1.0 - factor * factor);
    return smoothFactor * smoothFactor;
}

// 距离衰减：平滑窗口 × 1/d²；d² 下限 1e-4（1cm）防除零
fn getDistanceAttenuation(posToLight: vec3f, falloff: f32) -> f32 {
    let distanceSquare = dot(posToLight, posToLight);
    let attenuation = getSquareFalloffAttenuation(distanceSquare, falloff);
    return attenuation / max(distanceSquare, 1e-4);
}

struct PixelParams {
    diffuseColor: vec3f,
    f0: vec3f,
    roughness: f32,
}

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

// perceptualRoughness 已在材质换算端 clamp 到 ≥0.045。
// 坑：reflectance 是材质原值，必须先经 computeDielectricF0 换成 F0——直接传
// 0.5 会让 F0 = 0.5（应为 0.04），受光面整片叠白色高光、颜色发灰。
fn getStandardPixelParams(albedo: vec3f, metallic: f32, perceptualRoughness: f32, reflectance: f32) -> PixelParams {
    var pixel: PixelParams;
    pixel.roughness = perceptualRoughness * perceptualRoughness;
    pixel.diffuseColor = computeDiffuseColor(vec4f(albedo, 1.0), metallic);
    pixel.f0 = computeF0(vec4f(albedo, 1.0), metallic, computeDielectricF0(reflectance));
    return pixel;
}

// diffuse/specular 共用同一个 NoL（光源侧已 saturate）；occlusion（AO）乘在光源贡献上
fn surfaceShading(pixel: PixelParams, light_l: vec3f, light_colorIntensity: vec4f, light_attenuation: f32, light_NoL: f32, V: vec3f, n: vec3f, NoV: f32, occlusion: f32) -> vec3f {
    let h = normalize(V + light_l);
    let NoL = light_NoL;
    let NoH = saturate(dot(n, h));
    let LoH = saturate(dot(light_l, h));

    // 镜面项
    let roughness = pixel.roughness;
    let D = D_GGX(roughness, NoH, h);
    let Vterm = V_SmithGGXCorrelated(roughness, NoV, NoL);
    // f90 = saturate(16.5·Σf0)：介电体饱和到 1，深色金属掠射高光变暗
    let f90 = saturate(dot(pixel.f0, vec3f(50.0 * 0.33)));
    let F = F_Schlick(pixel.f0, f90, LoH);
    let Fr = (D * Vterm) * F;

    // 漫反射项
    let Fd = pixel.diffuseColor * Fd_Lambert();

    let color = Fd + Fr;
    return (color * light_colorIntensity.rgb)
        * (light_colorIntensity.w * light_attenuation * NoL * occlusion);
}

// 方向光 L：UBO dir_to_light 归一化，零向量兜底朝上。
fn safe_light_dir(l_raw: vec3f) -> vec3f {
    return select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
}
