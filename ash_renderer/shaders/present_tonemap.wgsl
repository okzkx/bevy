// present tonemap（全屏）：HDR 中间目标 + Bloom 叠加 → 色调映射 → swapchain。
// 本 shader 全程线性，swapchain 的 SRGB view 在 ROP 写出时做唯一一次编码。
//
// 曲线（bloom_params.curve，A/B 挂 bloom_set / 管线控制窗口）：
// curve=0 Narkowicz 2015 逐通道拟合（对照基线）；
// curve=1 Filament 完整版 aces::ACES——十步管线函数级照抄、命名保留原名
//   （filament/src/ToneMapper.cpp:133-193 + ColorSpaceUtils.h @ 373190d49），
//   契约域 linear Rec.2020 进出（ToneMapper.h），本 shader 的 HDR 是 linear
//   sRGB，进曲线前 sRGB→Rec2020、出曲线后 Rec2020→sRGB 各转一次。
// gain = 亮度增益位（ap1 *= brightness；ACES=1.0，LEGACY 形态=1.667），与曝光
// 锚点（common/brdf.wgsl 的 EXPOSURE）分离外露。
//
// 接口：set0 = 帧 UBO（mode 旁路面）；set1 = G-buffer 采样集（b2 hdr）；set2 =
// bloom 参数集（b0 up0 / b1 合法闲置 / b2 采样器 / b3 参数 UBO）。
// debug 仪器旁路：mode != 0 时 HDR 内是直出可视化——跳过 bloom 叠加与色调映射
//（写后读面失真即仪器失真）。
// #import frame
// #import fullscreen

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(2) var hdr_tex: texture_2d<f32>;
@group(1) @binding(4) var blit_sampler: sampler;
@group(2) @binding(0) var bloom_tex: texture_2d<f32>;
@group(2) @binding(2) var bloom_sampler: sampler;
@group(2) @binding(3) var<uniform> bloom_params: BloomParams;

struct BloomParams {
    strength: f32,
    enabled: f32,
    tonemap: f32,
    curve: f32,
    gain: f32,
}

// Narkowicz ACES 拟合（线性域输入/输出，编码在 SRGB view ROP 恰好一次）
fn aces_narkowicz(x: vec3f) -> vec3f {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3f(0.0), vec3f(1.0));
}

// —— Filament 完整版 aces::ACES 照抄区（ToneMapper.cpp:133-193 +
// ColorSpaceUtils.h @ 373190d49，函数级移植命名保留原名；输入输出
// linear Rec.2020）——————————————————————————————————

const RAD_TO_DEG: f32 = 57.29577951308232;

// 色域基底矩阵（ColorSpaceUtils.h 列主序逐值照抄，每行一列；四个合成矩阵
// 用同源合成式编译期展开：sRGB_to_Rec2020 = XYZ_to_Rec2020 * sRGB_to_XYZ 等）
const XYZ_TO_SRGB: mat3x3f = mat3x3f(
    3.2404542, -0.9692660, 0.0556434,
    -1.5371385, 1.8760108, -0.2040259,
    -0.4985314, 0.0415560, 1.0572252,
);
const SRGB_TO_XYZ: mat3x3f = mat3x3f(
    0.4124560, 0.2126730, 0.0193339,
    0.3575760, 0.7151520, 0.1191920,
    0.1804380, 0.0721750, 0.9503040,
);
const XYZ_TO_REC2020: mat3x3f = mat3x3f(
    1.7166634, -0.6666738, 0.0176425,
    -0.3556733, 1.6164557, -0.0427770,
    -0.2533681, 0.0157683, 0.9422433,
);
const REC2020_TO_XYZ: mat3x3f = mat3x3f(
    0.6369530, 0.2626983, 0.0000000,
    0.1446169, 0.6780088, 0.0280731,
    0.1688558, 0.0592929, 1.0608272,
);
const AP1_TO_AP0: mat3x3f = mat3x3f(
    0.6954522414, 0.0447945634, -0.0055258826,
    0.1406786965, 0.8596711185, 0.0040252103,
    0.1638690622, 0.0955343182, 1.0015006723,
);
const AP0_TO_AP1: mat3x3f = mat3x3f(
    1.4514393161, -0.0765537734, 0.0083161484,
    -0.2365107469, 1.1762296998, -0.0060324498,
    -0.2149285693, -0.0996759264, 0.9977163014,
);
const AP1_TO_XYZ: mat3x3f = mat3x3f(
    0.6624541811, 0.2722287168, -0.0055746495,
    0.1340042065, 0.6740817658, 0.0040607335,
    0.1561876870, 0.0536895174, 1.0103391003,
);
const XYZ_TO_AP1: mat3x3f = mat3x3f(
    1.6410233797, -0.6636628587, 0.0117218943,
    -0.3248032942, 1.6153315917, -0.0082844420,
    -0.2364246952, 0.0167563477, 0.9883948585,
);
const SRGB_TO_REC2020: mat3x3f = XYZ_TO_REC2020 * SRGB_TO_XYZ;
const REC2020_TO_SRGB: mat3x3f = XYZ_TO_SRGB * REC2020_TO_XYZ;
const REC2020_TO_AP0: mat3x3f = AP1_TO_AP0 * XYZ_TO_AP1 * REC2020_TO_XYZ;
const AP1_TO_REC2020: mat3x3f = XYZ_TO_REC2020 * AP1_TO_XYZ;
const LUMINANCE_AP1: vec3f = vec3f(0.272229, 0.674082, 0.0536895);

fn rgb_2_saturation(rgb: vec3f) -> f32 {
    let mi = min(min(rgb.r, rgb.g), rgb.b);
    let ma = max(max(rgb.r, rgb.g), rgb.b);
    return (max(ma, 1e-5) - max(mi, 1e-5)) / max(ma, 1e-2);
}

fn rgb_2_yc(rgb: vec3f) -> f32 {
    // YC = Y + K·Chroma 的亮度代理，ycRadiusWeight=1.75（RGB 111 → YC 1）
    let ycRadiusWeight = 1.75;
    let chroma = sqrt(rgb.b * (rgb.b - rgb.g) + rgb.g * (rgb.g - rgb.r) + rgb.r * (rgb.r - rgb.b));
    return (rgb.b + rgb.g + rgb.r + ycRadiusWeight * chroma) / 3.0;
}

fn sigmoid_shaper(x: f32) -> f32 {
    // 值域 0..1、覆盖 -2..+2 的 sigmoid
    let t = max(1.0 - abs(x / 2.0), 0.0);
    let y = 1.0 + sign(x) * (1.0 - t * t);
    return y / 2.0;
}

fn glow_fwd(ycIn: f32, glowGainIn: f32, glowMid: f32) -> f32 {
    if (ycIn <= 2.0 / 3.0 * glowMid) {
        return glowGainIn;
    } else if (ycIn >= 2.0 * glowMid) {
        return 0.0;
    }
    return glowGainIn * (glowMid / ycIn - 1.0 / 2.0);
}

fn rgb_2_hue(rgb: vec3f) -> f32 {
    // 几何色相角（度）；中性色无色相，返回 0（Filament 同款初值兜底）
    var hue = 0.0;
    if (!(rgb.x == rgb.y && rgb.y == rgb.z)) {
        hue = RAD_TO_DEG * atan2(sqrt(3.0) * (rgb.y - rgb.z), 2.0 * rgb.x - rgb.y - rgb.z);
        if (hue < 0.0) {
            hue = hue + 360.0;
        }
    }
    return hue;
}

fn center_hue(hue: f32, centerH: f32) -> f32 {
    var hueCentered = hue - centerH;
    if (hueCentered < -180.0) {
        hueCentered = hueCentered + 360.0;
    } else if (hueCentered > 180.0) {
        hueCentered = hueCentered - 360.0;
    }
    return hueCentered;
}

fn darkSurround_to_dimSurround(linearCV: vec3f) -> vec3f {
    let DIM_SURROUND_GAMMA = 0.9811;
    let XYZ = AP1_TO_XYZ * linearCV;
    let sum = XYZ.x + XYZ.y + XYZ.z;
    // XYZ_to_xyY：xy 分量归一，y 分量原样（分母 TINY 兜底同款）
    var xyY = vec3f(XYZ.xy / max(sum, 1e-5), XYZ.y);
    xyY.z = clamp(xyY.z, 0.0, 65504.0);  // half 上界
    xyY.z = pow(xyY.z, DIM_SURROUND_GAMMA);
    // xyY_to_XYZ
    let a = xyY.z / max(xyY.y, 1e-5);
    let XYZ2 = vec3f(xyY.x * a, xyY.z, (1.0 - xyY.x - xyY.y) * a);
    return XYZ_TO_AP1 * XYZ2;
}

fn ACES(color: vec3f, brightness: f32) -> vec3f {
    // Glow 模块常量
    let RRT_GLOW_GAIN = 0.05;
    let RRT_GLOW_MID = 0.08;
    // 红色修正常量
    let RRT_RED_SCALE = 0.82;
    let RRT_RED_PIVOT = 0.03;
    let RRT_RED_HUE = 0.0;
    let RRT_RED_WIDTH = 135.0;
    // 受控去饱和常量
    let RRT_SAT_FACTOR = 0.96;
    let ODT_SAT_FACTOR = 0.93;

    // 色域进：Rec.2020 → AP0
    var ap0 = REC2020_TO_AP0 * color;

    // Glow 模块：按饱和度加低强度光晕
    let saturation = rgb_2_saturation(ap0);
    let ycIn = rgb_2_yc(ap0);
    let s = sigmoid_shaper((saturation - 0.4) / 0.2);
    let addedGlow = 1.0 + glow_fwd(ycIn, RRT_GLOW_GAIN * s, RRT_GLOW_MID);
    ap0 = ap0 * addedGlow;

    // 红色修正：保护红色相不被压脏
    let hue = rgb_2_hue(ap0);
    let centeredHue = center_hue(hue, RRT_RED_HUE);
    var hueWeight = smoothstep(0.0, 1.0, 1.0 - abs(2.0 * centeredHue / RRT_RED_WIDTH));
    hueWeight = hueWeight * hueWeight;

    ap0.r = ap0.r + hueWeight * saturation * (RRT_RED_PIVOT - ap0.r) * (1.0 - RRT_RED_SCALE);

    // AP0 → AP1（RGB rendering space），clamp 到 half 上界
    let ap1 = clamp(AP0_TO_AP1 * ap0, vec3f(0.0), vec3f(65504.0));

    // 一级受控去饱和
    var ap1d = mix(vec3f(dot(ap1, LUMINANCE_AP1)), ap1, RRT_SAT_FACTOR);

    // 亮度增益位（ACES=1.0 / LEGACY=1.667）
    ap1d = ap1d * brightness;

    // Hill RRT+ODT 拟合（colour-unity CIECAM02 notebook）
    let a = 2.785085;
    let b = 0.107772;
    let c = 2.936045;
    let d = 0.887122;
    let e = 0.806889;
    let rgbPost = (ap1d * (a * ap1d + b)) / (ap1d * (c * ap1d + d) + e);

    // 暗包围补偿（dim surround）
    var linearCV = darkSurround_to_dimSurround(rgbPost);

    // 二级受控去饱和
    linearCV = mix(vec3f(dot(linearCV, LUMINANCE_AP1)), linearCV, ODT_SAT_FACTOR);

    // 色域出：AP1 → Rec.2020
    return AP1_TO_REC2020 * linearCV;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let hdr = textureSampleLevel(hdr_tex, blit_sampler, in.uv, 0.0).rgb;
    if (frame.mode != 0u || bloom_params.tonemap < 0.5) {
        // debug 仪器直出 / tonemap 关（A/B 的 B 面）——HDR 内容原样
        return vec4f(hdr, 1.0);
    }
    var bloom = vec3f(0.0);
    if (bloom_params.enabled > 0.5) {
        bloom = textureSampleLevel(bloom_tex, bloom_sampler, in.uv, 0.0).rgb
            * bloom_params.strength;
    }
    let color = hdr + bloom;
    if (bloom_params.curve < 0.5) {
        return vec4f(aces_narkowicz(color), 1.0);
    }
    let rec2020 = SRGB_TO_REC2020 * color;
    let mapped = ACES(rec2020, bloom_params.gain);
    return vec4f(REC2020_TO_SRGB * mapped, 1.0);
}
