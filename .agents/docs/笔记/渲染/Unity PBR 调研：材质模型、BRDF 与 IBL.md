# Unity PBR 调研：材质模型、BRDF 与 IBL

## 0. 判定线

- **URP 的直接光 BRDF 是移动端近似**：GGX 法向分布 × Kelemen-Szirmay-Kalos 形式的 Visibility×Fresnel 合并项（"Optimizing PBR for Mobile"，ARM SIGGRAPH 2015），不是完整 Cook-Torrance。HDRP 才用完整 GGX + Smith 项。自研渲染器对齐哪一档要先拍板。
- **roughness 两级换算**：smoothness ∈ [0,1] → perceptualRoughness = 1 − smoothness → **roughness = perceptual²（α）**，直接光与 IBL 全部以 α 参战。
- **金属度工作流的 F0 = lerp(0.04, albedo, metallic)，diffuse = albedo × (1 − reflectivity)**；介电体 F0 固定 0.04（kDielectricSpec）。
- **URP 的 IBL 镜面项用解析近似，不用 2D BRDF LUT**：specular 环境项 = (1/(α²+1)) × lerp(F0, F0 放大到掠射角, fresnel)，配合预滤波 cubemap 的 mip——这是 split-sum 的简化版。
- **能量守恒是结构性的**（diffuse×(1−reflectivity)），但 **URP 不做多重散射能量补偿**；metallic 高 smoothness 高时暗于物理正确值。

以下全部以 URP master 源码为准（Unity-Technologies/Graphics 仓库），文件均已逐行核对。

## 1. 材质参数模型

来源：[BRDF.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/BRDF.hlsl)

### 1.1 两种工作流

`InitializeBRDFData` 用 `_SPECULAR_SETUP` 关键字二选一：

- **Metallic 工作流（默认）**：
  - reflectivity = 1 − OneMinusReflectivityMetallic(metallic)，其中 oneMinusReflectivity = (1−0.04) − metallic×(1−0.04)
  - brdfDiffuse = albedo × oneMinusReflectivity
  - **brdfSpecular = lerp(float3(0.04, 0.04, 0.04), albedo, metallic)**——金属的 F0 就是 albedo，非金属固定 0.04，中间态线性插值（物理上不严格，工业界通行）
- **Specular 工作流**：specular 贴图直接当 F0，brdfDiffuse = albedo × (1 − max(specular.r,g,b))。适合艺术控制，容易做出不物理的值。

### 2. BRDFData 预计算结构

光照循环前把不变量折进 `BRDFData`，避免每盏灯重算：

```
perceptualRoughness = 1 - smoothness
roughness (α)       = max(perceptual², HALF_MIN_SQRT)   // 防 half 下溢
roughness2          = α²
grazingTerm         = saturate(smoothness + reflectivity)   // 掠射角菲涅尔的 F0 上界
normalizationTerm   = α*4 + 2           // 折进 MAD 的合并项，见下
roughness2MinusOne  = α² - 1
```

`_ALPHAPREMULTIPLY_ON`（半透明）时 diffuse 预乘 alpha——透明物体的透射只作用于 diffuse 项，specular 保持全量。

### 3. 直接光 BRDF（移动端近似，逐项拆解）

`DirectBRDFSpecular` 计算 **D×V×F/4 的合并近似**：

```hlsl
// D = α² / (NoH² (α²-1) + 1)²            —— 标准 GGX
// V×F ≈ 1 / (LoH² (α + 0.5))             —— Kelemen-Szirmay-Kalos 改形 + 1/LoH² 菲涅尔近似
// 合并后乘 normalizationTerm = α*4 + 2（= (α+0.5)*4 改写成 MAD）
float d = NoH*NoH * roughness2MinusOne + 1.00001;
half specularTerm = roughness2 / (d*d * max(0.1, LoH*LoH) * normalizationTerm);
```

要点：

- **V 项不是 Smith 几何项**，是 Kelemen-Szirmay-Kalos 近似与菲涅尔 1/LoH² 合并的"minimalist Cook-Torrance"。对低粗糙度近似不错，中高粗糙度与完整 Smith-GGX 有可见偏差。
- `max(0.1, LoH²)` 是 half 精度防溢出的经验下限；half 平台上 specularTerm 还会 clamp 到 [0, 1000]（注释写明：给半精度留容量，上限从 100 提到 1000 以贴近 float 路径观感）。
- 最终光照 = `brdfData.diffuse * lightColor * NoL + specularTerm * brdfData.specular * lightColor * NoL`——diffuse 项就是 Lambert（无 Disney 漫反射），没有 rims/复杂项。

**HDRP 对比**：HDRP 的 Lit 走完整 GGX D、Smith 高度相关可见性 V、Schlick 菲涅尔 F，并按材料类型（标准/布料/清漆）换 BRDF；本文以 URP 为锚点，写自研渲染器时若追求质量可直接上完整项，差距主要在中高粗糙度的高光形状与强度。

### 4. 间接光（IBL）

来源：[GlobalIllumination.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/GlobalIllumination.hlsl)

URP 的环境项 = 漫反射（SH/探针/光贴图）+ 镜面（预滤波 cubemap + 解析 BRDF 项），即 split-sum 思想，但两半都取了便宜的近似：

```hlsl
// 镜面环境项（EnvironmentBRDFSpecular）：
surfaceReduction = 1.0 / (α² + 1.0)
specularEnv = surfaceReduction * lerp(F0, grazingTerm, fresnelTerm)
```

- 对照 Karis 的 split-sum（2D LUT 积分微面元 BRDF），URP 用的是**解析拟合**：粗糙度只进 surfaceReduction 一个标量，菲涅尔用 grazingTerm 做掠射放大。省一张 LUT 和一次采样，代价是各向同性近似、边缘处比 LUT 略偏。
- **mip 选择**：`mip = PerceptualRoughnessToMipmapLevel(perceptualRoughness)`，标准曲线 `perceptual * (1.7 - 0.7*perceptual) * levelCount`（core 的 ImageBasedLighting.hlsl）——注意曲线输入是 perceptual roughness 而不是 α。
- **反射探针系统**（`CalculateIrradianceFromReflectionProbes`）：
  - 每对象支持两个 cubemap（unity_SpecCube0/1）按**包围盒权重**混合：weight = 各轴到盒面距离的最小值 saturate；主导探针（importance 更高或体积更小）会压缩另一方的权重，总权重不足 0.99 的余量交给全局环境 cubemap。
  - **盒投影**（BoxProjectedCubemapDirection）：反射向量与盒面求交把采样方向拉回盒内空间，修正"无限远 cubemap 在室内穿帮"。
  - GPU Resident Drawer 路径下 lightmap 走**纹理数组**（unity_Lightmaps + unity_LightmapIndex 间接），否则走单张纹理——两种绑定并存，数组路线减少 batch 破碎但在部分 GPU 上不友好，这正是我们 bindless 表可以全吃掉的分支。
- **SSR**：`GlossyEnvironmentReflection` 里 SSR 结果按 alpha 与探针项 lerp，SSR 未覆盖回退探针。

### 5. 环境漫反射：SH 与光贴图

同一文件里能看到 URP 的漫反射环境四个来源按宏分派：

1. **SH 探针**：`SampleSHVertex/SampleSHPixel`——L2 逐顶点算、L0+L1 逐像素算（EVALUATE_SH_MIXED），或全逐像素；unity_SHAr/Ag/Ab/Br/Bg/Bb/C 九个寄存器。
2. **光贴图**：`SampleSingleLightmap`（非方向）/ `SampleDirectionalLightmap`（方向光贴图，lightmap + 存法线分布的 indirection 贴图配对采样）。GPU-driven 模式下是纹理数组。
3. **APV 探针体**：`EvaluateAdaptiveProbeVolume`，逐像素采样，可带 probe occlusion（见谱系篇）。
4. **屏幕空间辐照度**（`_SCREEN_SPACE_IRRADIANCE`）：HDRP 式 SSGI 输出的屏幕空间间接光直接 LOAD——不透明表面用之，透明回退环境探针。

### 6. 光照循环与衰减

来源：[RealtimeLights.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/RealtimeLights.hlsl)

- **主光 + 附加光**：主光单独（实时阴影走阴影贴图），附加光逐像素循环（可选用结构化 buffer `_AdditionalLightsBuffer`，即 clustering/forward+ 路径）。
- **距离衰减**（与 Unity lightmapper 一致，平滑到 range 归零）：

```hlsl
lightAtten   = rcp(distanceSqr)
smoothFactor = saturate(1 - (distanceSqr * x)²); smoothFactor²    // x = 1/range²，预烘焙进 half2
return lightAtten * smoothFactor
```

- **聚光角衰减**：线性插值 (SdotL − cosOuter)/(cosInner − cosOuter) 重排为 MAD（SdotL·a + b），**平方**后输出——内锥全亮、外锥平滑到零。
- **ShadowMask / subtractive**：`SubtractDirectMainLightFromLightmap` 把主光的 Lambert 贡献乘 (1−shadowAttenuation) 从烘焙 GI 里**减回去**，再 clamp 到 _SubtractiveShadowColor，最后取原值与减后值的较暗者——这就是"烘焙含太阳的 lightmap + 实时阴影"能在同一物体上并存的机制。

### 7. 能量守恒与多重散射

- URP 直接光里 diffuse/specular 的能量分配靠 `(1−reflectivity)` 一步去除，没有 specular 与 diffuse 之间的二阶能量守恒（高光强时漫反射不会相应变暗），也**没有多重散射补偿**：低粗糙度金属会偏暗。
- Kajiya 的做法可作参照：反射走 VNDF 采样，VNDF 生成的方向无效（指进表面）时**重掷**（多重散射的射线分布仍近似原 BRDF 形状，Heitz 等 2016），能量守恒由末端预积分项补，并额外补金属多重散射带来的饱和度上升。
- 验证手段：**白炉测试**（纯白环境 + 纯白材质，不同 roughness/metallic 下画面应保持不变且不超曝）——自研渲染器上 IBL 管线的第一道验收。

## 8. 对 ash_renderer 的落地清单

1. **材质模型对齐 URP metallic 工作流**：albedo(sRGB→线性)、metallic、smoothness、occlusion 四通道，材质环里已有的数据布局可直接扩展；F0=lerp(0.04, albedo, metallic) 一行。
2. **直接光 BRDF 建议直接上完整 GGX + Smith 高度相关项 + Schlick 菲涅尔**（WGSL 下无 half 精度压力，URP 的近似是给移动 half 逼出来的；完整项公式量级相同）。
3. **IBL 基建三件**：预滤波环境 cubemap（roughness→mip，GGX 重要性采样预卷积）、BRDF 积分项（选择：先抄 URP 解析近似零基建起步，后换 2D LUT 提质量）、mip 曲线按 perceptual 算。
4. **sRGB 线性域纪律**：albedo/env 输入 sRGB 解码，光照全部线性，输出一次编码（与库内 swapchain UNORM+SRGB view 的既定方向一致）。
5. **探针结构预留**：SH 环境漫反射（L0L1）是所有 GI 谱系篇方案的公共查询接口——先实现"从一组 SH 探针插值漫反射"的着色路径，GI 数据后面换（lightmap→APV→动态缓存）着色层不用动。

## 来源清单

- [URP BRDF.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/BRDF.hlsl)（直接光/预计算/金属工作流，逐行核对）
- [URP GlobalIllumination.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/GlobalIllumination.hlsl)（IBL/探针混合/盒投影/SH/lightmap）
- [URP RealtimeLights.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/RealtimeLights.hlsl)（衰减函数/光照抽象）
- [kajiya GI overview](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)（多重散射 VNDF 重掷与预积分补偿）
- "Optimizing PBR for Mobile"（ARM，SIGGRAPH 2015 Mobile Course）——URP 注释中注明的 V×F 近似出处
