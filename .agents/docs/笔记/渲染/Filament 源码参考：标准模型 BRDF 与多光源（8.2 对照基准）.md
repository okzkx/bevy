# Filament 源码参考：标准模型 BRDF 与多光源（8.2 对照基准）

> 对应 [8.2 段 README 参考底座](../../8-完整延迟渲染管线/8.2-FilamentBRDF与多光源/README.md)、[Unity PBR 调研](Unity%20PBR%20调研：材质模型、BRDF%20与%20IBL.md)。版本钉：本地检出 `F:\okzkx\filament`，HEAD `373190d49`（`git describe` = rc/1.78.1-cut-31-g373190d49，移动开发线，函数行号会漂移，函数名不会）。全部引用逐行核对于 2026-10-22。

## 0. 判定线

- **8.2 对照基准钉明（desktop 默认组合，实测核实）**：`D_GGX`（GGX 法向分布）＋ `V_SmithGGXCorrelated`（Heitz 2014 高度相关完整项）＋ `F_Schlick(f0, f90, VoH)`（f90 = saturate(16.5·Σf0)，介电体饱和到 1）＋ **diffuse = Lambert（1/π）**。**Burley 不是默认**——它是 `BRDF_DIFFUSE` 宏的另一个可选值，8.2 不取。
- desktop 质量档 = `FILAMENT_QUALITY_HIGH`（`ShaderModel::DESKTOP` → quality_high，libs/filamat/src/shaders/CodeGenerator.cpp），所以 V 项默认走**完整 Smith** 而非 FAST 近似；FAST（Hammon 2017）是 NORMAL 以下档的替代。
- **glTF 的 roughnessFactor 就是 perceptualRoughness**（surface_shading_lit.fs `perceptualRoughness = material.roughness`），着色用 α = perceptual²，下限 clamp 0.045（desktop `MIN_PERCEPTUAL_ROUGHNESS`）。即：贴图/参数域记 perceptual，进 D/V 前平方——与 Unity 篇"两级换算"同构。
- F0 = `baseColor·metallic + reflectance·(1−metallic)`，reflectance 默认 0.5 → 介电体 F0 = 0.16·0.5² = **0.04**；diffuseColor = `baseColor·(1−metallic)`。与 Unity 篇 `lerp(0.04, albedo, metallic)` 在 reflectance=0.5 时逐项等价。
- **NoL 不进单个 lobe**：`surfaceShading` 返回 `(Fd + Fr)`，方向光/点光的 NoL 在尾部一次性乘（`colorIntensity.rgb × intensity × attenuation × NoL × occlusion`）。
- 金属/非金属多光源循环提前退出：`NoL ≤ 0 || attenuation ≤ 0` 直接 continue，不积累颜色。
- 强度单位有物理定义：方向光 **lux**（照度）、点光/聚光 **candela**（发光强度），`colorIntensity.w = intensity × exposure`（预曝光）。本项目 bevy `DirectionalLight.illuminance` 同为 lux，可直接对接。
- 8.2 取 `energyCompensation = 1.0`：Filament 的该项来自 IBL 的预滤波 DFG LUT（`1.0 + f0·(1/dfg.y − 1)`），补偿多重散射能量损失；无 IBL 基建时只能取 1，误差方向 = 高粗糙金属高光略暗（与 Filament 源内 TODO "attenuate the diffuse lobe" 同状态）。

## 1. BRDF 宏默认组合

`shaders/src/surface_brdf.fs` 头部：

```glsl
#define BRDF_DIFFUSE                DIFFUSE_LAMBERT      // Burley 是可选，默认 Lambert

#if FILAMENT_QUALITY < FILAMENT_QUALITY_HIGH
#define BRDF_SPECULAR_V             SPECULAR_V_SMITH_GGX_FAST
#else
#define BRDF_SPECULAR_V             SPECULAR_V_SMITH_GGX // desktop 默认走这里
#endif
#define BRDF_SPECULAR_D             SPECULAR_D_GGX       // 两档相同
#define BRDF_SPECULAR_F             SPECULAR_F_SCHLICK   // 两档相同
```

质量档由 matc/ShaderModel 决定：desktop 默认 HIGH，mobile 默认 LOW。LOW 档的菲涅尔还把 f90 固定为 1（省一次点积）。

## 2. 材质参数面（进入光照前的一次性换算）

来源：`shaders/src/surface_material.fs`、`shaders/src/surface_shading_lit.fs`（getStandardPixelParams）。

```glsl
float perceptualRoughnessToRoughness(float p) { return p * p; }   // α = perceptual²
#define MIN_PERCEPTUAL_ROUGHNESS 0.045    // desktop；mobile 0.089（fp16 逼出）
#define MIN_ROUGHNESS            0.002025 // = 0.045²
#define MIN_N_DOT_V 1e-4                 // clampNoV，Neubelt/Pettineo

pixel.perceptualRoughness = clamp(material.roughness, MIN_PERCEPTUAL_ROUGHNESS, 1.0);
pixel.roughness           = perceptualRoughnessToRoughness(pixel.perceptualRoughness);
pixel.diffuseColor        = computeDiffuseColor(baseColor, metallic);  // = baseColor·(1−metallic)
pixel.f0                  = computeF0(baseColor, metallic, reflectance); // reflectance 默认 0.5
```

- roughness 纹理采样后同样进 perceptual 域（`material.roughness` ← roughnessFactor/roughness 贴图）。
- NoV 全程用 clamp 后的 `shading_NoV`（≥1e-4），防止掠射除零。

## 3. 直接光 BRDF 逐项（对照函数清单）

来源：`shaders/src/surface_brdf.fs`，全部为 desktop 默认路径。**逐项公式即 8.2 判定线①的对照基准**。

### 3.1 D_GGX（Walter et al. 2007）

```glsl
float D_GGX(float roughness, float NoH, const vec3 h) {
    float oneMinusNoHSquared = 1.0 - NoH * NoH;      // mobile 用 cross(N,h) Lagrange 恒等式防 fp16 精度丢失
    float a = NoH * roughness;
    float k = min(roughness / (oneMinusNoHSquared + a * a), 453.5); // 453.5 防 fp16 溢出
    return k * (k * (1.0 / PI));
}
```

与教科书形式 `α² / (π·(NoH²(α²−1)+1)²)` **代数等价**（分母 1−NoH²(1−α²) = NoH²(α²−1)+1），Filament 只是重排成 k²/π 减少除法。WGSL float 路径两种形态都可，单测对照时应收敛到同一值。

### 3.2 V_SmithGGXCorrelated（Heitz 2014 高度相关）

```glsl
float V_SmithGGXCorrelated(float roughness, float NoV, float NoL) {
    float a2 = roughness * roughness;
    float lambdaV = NoL * sqrt((NoV - a2 * NoV) * NoV + a2);
    float lambdaL = NoV * sqrt((NoL - a2 * NoL) * NoL + a2);
    return 0.5 / max(lambdaV + lambdaL, 0.0000077);   // PREVENT_DIV0 展开
}
```

注意 a2 = **α²**（α 已是 perceptual²，所以 a2 是四次方量级）。FAST 档（Hammon 2017）为 `0.5 / mix(2·NoL·NoV, NoL+NoV, roughness)`——8.2 不取，但对照时可用于排除实现错误。

### 3.3 F_Schlick（双参版）

```glsl
vec3 F_Schlick(const vec3 f0, float f90, float VoH) {
    return f0 + (f90 - f0) * pow5(1.0 - VoH);
}
// 非 SPECULAR_COLOR_FACTOR 路径（standard 默认）：
float f90 = saturate(dot(f0, vec3(50.0 * 0.33)));    // = saturate(16.5·Σf0)
```

介电体 f0=0.04 → f90 饱和到 1.0；深色金属 f0→0 时 f90 同步趋 0（掠射高光变暗，物理正确）。

### 3.4 diffuse（默认 Lambert）

```glsl
float Fd_Lambert() { return 1.0 / PI; }
// 可选 Burley（f90 = 0.5 + 2·α·LoH²，F_Schlick(1, f90, NoL)·F_Schlick(1, f90, NoV)/π）
```

Filament 的 diffuse lobe = `diffuseColor × Fd_Lambert()`，无 roughness 依赖。本项目现行 Lambert（`gbuffer_composite.wgsl` 的 `lambert × INV_PI`）与之**数学上完全同款**——8.2 换 BRDF 时 diffuse 项零改动，只加 specular lobe。

## 4. surfaceShading 组织（光照累积 pass 的同构形状）

来源：`shaders/src/surface_shading_model_standard.fs`（surfaceShading）。

```glsl
vec3 surfaceShading(const PixelParams pixel, const Light light, float occlusion) {
    vec3 h = normalize(shading_view + light.l);
    float NoV = shading_NoV;                // 已 clamp 1e-4
    float NoL = light.NoL;                  // 光源侧已 saturate
    float NoH = saturate(dot(shading_normal, h));
    float LoH = saturate(dot(light.l, h));

    vec3 Fr = specularLobe(pixel, light, h, NoV, NoL, NoH, LoH) * pixel.energyCompensation;
    vec3 Fd = diffuseLobe(pixel, NoV, NoL, LoH);      // diffuseColor × Fd_Lambert()
    vec3 color = Fd + Fr;
    return (color * light.colorIntensity.rgb)
         * (light.colorIntensity.w * light.attenuation * NoL * occlusion);
}
```

要点：

- **逐光源只做一次 h 归一化和四个点积**，D/V/F 全部吃这组余弦；参数换算（§2）在光照循环外做一次。
- NoL 折在尾部，diffuse 与 specular 共享同一个 NoL 乘法。
- occlusion（AO）是**乘在光源贡献上的标量**（8.2 的 AO 贴图槽对接点）；环境 AO 是另一条路（multiBounceAO，归间接光）。
- 本项目 8.1 的 `gbuffer_composite.wgsl` 已经是"全屏 pass 读 G-buffer × 光源"的骨架，8.2 把其中的 Lambert 行换成本函数同构即可，透明层（8.1.3 cluster forward）用同一份 lobe 函数。

## 5. 光源求值与驻留（头号待决的参考证据）

来源：`shaders/src/surface_lighting.fs`（Light/PixelParams 结构）、`surface_light_directional.fs`、`surface_light_punctual.fs`。

### 5.1 Light 结构（shadow 位预留的先例）

```glsl
struct Light {
    vec4 colorIntensity;   // rgb 颜色，w = 预曝光后的强度
    vec3 l;                // 指向光源的单位向量
    float attenuation;     // 距离/角度衰减（方向光恒 1）
    float NoL;             // saturate(dot(n, l))
    bool castsShadows;  int shadowIndex;   // 阴影标志位 + 索引——与 8.2 "光源记录预留 shadow 标志位"同构
    uint channels;         // 光照通道位，与物体通道位做与运算过滤
};
```

Filament 用通道位（`light.channels & objectChannels == 0` 就跳过）做光照分层；8.2 若需要"这盏灯影响这层物体"，这是现成参照。

### 5.2 衰减公式（punctual）

```glsl
// 距离衰减：smooth 因子平方 + 1/d²（在调用点除）
float getSquareFalloffAttenuation(float distanceSquare, float falloff) {
    float factor = distanceSquare * falloff;              // falloff = 1/radius²
    float smoothFactor = saturate(1.0 - factor * factor);
    return smoothFactor * smoothFactor;
}
float getDistanceAttenuation(const vec3 posToLight, float falloff) {
    float distanceSquare = dot(posToLight, posToLight);
    float attenuation = getSquareFalloffAttenuation(distanceSquare, falloff);
    // 远距全局衰减（lightFarAttenuationParams 帧常量）……
    return attenuation / max(distanceSquare, 1e-4);       // 1cm 体积防除零
}
// 聚光角衰减：内锥全亮、外锥平滑到零，输出平方
float getAngleAttenuation(const vec3 lightDir, const vec3 l, const vec2 scaleOffset) {
    float cd = dot(lightDir, l);
    float attenuation = saturate(cd * scaleOffset.x + scaleOffset.y);
    return attenuation * attenuation;
}
```

### 5.3 驻留形态：Filament 的证据

- **灯参数本体走每帧更新的 UBO**（`lightsUniforms`，每灯打包一个 mat4：position+radius / direction / color(half4×2)+角度 scaleOffset / intensity+type+shadow+channels），帧间由 CPU 侧 LightManager 组装上传——**没有"常驻灯表"**；远距衰减等帧级参数也在帧 UBO 里。
- 空间加速是 **froxel**（视锥体素 3D 分簇）：froxel 纹理存 recordOffset+count，record 缓冲存灯索引，逐片元查 froxel → 遍历灯列表。这与 8.1 已建的 3D cluster 光源列表**同族不同实现**（Filament 靠 froxelizer 每帧 CPU 侧分簇，8.1 是 compute 侧组装）。
- **对 8.2 头号待决（常驻表 vs 每帧环）**：Filament 的行业证据支持"灯参数每帧上传 + 空间索引加速"的形态；灯数在现代场景里是百量级、每灯 64B，每帧上传量 ≪ 贴图流送。常驻表（set2 式）省上传但要解决灯增删的槽位管理—— Filament 没走这条路本身就是一个参考信号。

## 6. ash_renderer 落地映射清单（8.2 施工钉）

1. **WGSL 对照函数清单（判定线①基准）**：`D_GGX`、`V_SmithGGXCorrelated`、`F_Schlick(f0, f90, VoH)`（f90 = saturate(16.5·Σf0)）、`Fd_Lambert()`。命名对照：Filament `shading_view`/`light.l`/`h` ↔ 本项目 `V`/`L`/`H`；`NoV/NoL/NoH/LoH` 同名。D 项可选教科书形式或 k²/π 形式（代数等价，单测对照收敛）。
2. **材质环扩列换算链**：glTF roughness → perceptualRoughness（域内 clamp 0.045）→ α = p²；F0 = `baseColor·m + 0.04·(1−m)`；diffuseColor = `baseColor·(1−m)`；AO 贴图 → occlusion（乘光源贡献尾部）。
3. **energyCompensation = 1.0**，DFG LUT 与多重散射补偿随 IBL（步骤 9 附近）另立。
4. **光照累积 pass**：8.1 composite 骨架上把 Lambert 换成 §4 同构 `(Fd+Fr)·NoL`，光源数据从 set1 UBO 单方向光扩成列表（驻留形态=头号待决，§5.3 证据供拍板）；ambient 项保持在光源循环外。
5. **透明层复用**：8.1.3 透明 forward 与不透明光照累积共用同一份 lobe 函数（材质参数面一致是前提）。
6. **单位与曝光**：方向光 lux / punctual candela；`intensity × exposure` 预曝光 vs 本项目尾部 `× EXPOSURE` 常量——标量位置等价，8.4 正式 HDR 时再统一到预曝光口径。

## 7. 边界（本篇不主张）

- **照抄范围 = BRDF 数学与光源定义**；pass 组织不照抄——Filament 是 forward（逐片元查 froxel 逐灯算），8.2 是 deferred 光照累积，只有"逐光源累加到 inout color"的循环形状同构。
- standard 以外的 shading model（cloth/clearcoat/subsurface/refraction）不在 8.2 交付；本篇只记录了它们存在与入口（surface_shading_model_*.fs）。
- 间接光全家（prefilteredDFG、irradiance SH、multiBounceAO、reflected lobe 拉伸）不在 8.2，接口位已留（§6.3）。
- SUN_AS_AREA_LIGHT（HIGH 档把太阳做圆盘面光采样）不取——方向光按纯方向光对照，这是 8.2 判定线①的对照口径。

## 来源清单

- `shaders/src/surface_brdf.fs`——宏默认组合、D/V/F/diffuse 全函数（逐行核对）
- `shaders/src/surface_material.fs`——MIN_ROUGHNESS/clampNoF/computeDiffuseColor/computeF0/roughness 换算
- `shaders/src/surface_shading_lit.fs`——getStandardPixelParams（roughness 流入与 clamp 链）、energyCompensation
- `shaders/src/surface_shading_model_standard.fs`——surfaceShading/isotropicLobe/diffuseLobe 组织
- `shaders/src/surface_lighting.fs`——Light/PixelParams 结构定义
- `shaders/src/surface_light_directional.fs` / `surface_light_punctual.fs`——方向光/点光求值、衰减公式、froxel 查询
- `libs/filamat/src/shaders/CodeGenerator.cpp`——ShaderModel → FILAMENT_QUALITY 映射（DESKTOP→HIGH）
- `filament/include/filament/LightManager.h`——强度单位定义（lux/candela）
- `libs/gltfio/src/AssetLoader.cpp`——glTF roughness_factor/metallic_factor 直传材质参数
- 本地版本：`F:\okzkx\filament` @ `373190d49`（rc/1.78.1-cut-31-g373190d49）
