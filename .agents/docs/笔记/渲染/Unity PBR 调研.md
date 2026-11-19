# Unity PBR 调研：材质模型、BRDF 与 IBL

同一个 albedo（表面基础颜色），填给金属和填给塑料，得到的反射率完全不同：URP（Unity 的通用渲染管线）的金属工作流里 F0（垂直入射时的反射率）= lerp(0.04, albedo, metallic)（lerp 是线性插值），metallic（金属度）=1 时 F0 就是 albedo；介电体（非金属）不管 albedo 多亮，F0 固定 0.04（kDielectricSpec）。`InitializeBRDFData` 把这条换算写成一行，后面的直接光和 IBL（用环境贴图当光源的间接光）都拿这个 F0 当镜面项输入。

**URP 的 PBR（基于物理的渲染）是移动端可负担的近似：金属和介电的反射率用一个 lerp 统一，直接光把法线分布、可见性、菲涅尔三项压成一个合并近似，环境镜面反射用解析式代替预积分查找表——自研渲染器要先决定对齐 URP 这一档，还是上完整的微表面模型。**

## 1. 材质参数模型

以下全部以 URP master 源码为准（Unity-Technologies/Graphics 仓库），文件均已逐行核对。

来源：[BRDF.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/BRDF.hlsl)——直接光、预计算结构与金属工作流的逐行核对对象；BRDF 是描述"从某个入射方向来的光有多少反射到某个出射方向"的函数。

### 1.1 两种工作流

metallic 是金属度（0 表示介电，1 表示金属），smoothness 是光滑度，reflectivity 是反射率。`InitializeBRDFData` 用 `_SPECULAR_SETUP` 关键字二选一：

- **Metallic 工作流（默认）**：
  - reflectivity = 1 − OneMinusReflectivityMetallic(metallic)，其中 oneMinusReflectivity = (1−0.04) − metallic×(1−0.04)
  - brdfDiffuse = albedo × oneMinusReflectivity，也就是 diffuse = albedo × (1 − reflectivity)
  - **brdfSpecular = lerp(float3(0.04, 0.04, 0.04), albedo, metallic)**——金属的 F0 就是 albedo，非金属固定 0.04，中间态线性插值（物理上不严格，工业界通行）
- **Specular 工作流**：specular 贴图直接当 F0，brdfDiffuse = albedo × (1 − max(specular.r,g,b))。适合艺术控制，容易做出不物理的值。

## 2. BRDFData 预计算结构

roughness 有两级换算：smoothness ∈ [0,1] → perceptualRoughness = 1 − smoothness → roughness = perceptual²（记作 α），直接光与 IBL 的粗糙度都用 α。光照循环前把不变量折进 `BRDFData`，避免每盏灯重算：

```
perceptualRoughness = 1 - smoothness
roughness (α)       = max(perceptual², HALF_MIN_SQRT)   // 防 half 下溢
roughness2          = α²
grazingTerm         = saturate(smoothness + reflectivity)   // 掠射角菲涅尔的 F0 上界
normalizationTerm   = α*4 + 2           // 折进 MAD 的合并项，见下
roughness2MinusOne  = α² - 1
```

`half` 是半精度浮点，`saturate` 是钳到 [0,1]，`MAD` 是乘加指令，`grazingTerm` 是掠射角方向的反射率上界。`_ALPHAPREMULTIPLY_ON`（半透明）时 diffuse 预乘 alpha——透明物体的透射只作用于 diffuse 项，specular 保持全量。

## 3. 直接光 BRDF（移动端近似，逐项拆解）

先把四个名字说开：GGX 是一种微表面法线分布，决定高光斑的形状；Smith 是几何遮挡项，描述微表面互相遮挡的比例；Kelemen-Szirmay-Kalos 是把可见性 V 与菲涅尔 F 合并成一个近似项的做法；Cook-Torrance 是完整的微表面 BRDF 框架，由 D（法线分布）、V/G（几何遮挡）、F（菲涅尔，反射率随观察角度变化的项）三部分组成。

URP 的直接光 BRDF 是移动端近似：GGX 法向分布 × Kelemen-Szirmay-Kalos 形式的 Visibility×Fresnel 合并项（"Optimizing PBR for Mobile"，ARM SIGGRAPH 2015），不是完整 Cook-Torrance。HDRP（Unity 的高清渲染管线）才用完整 GGX + Smith 项——自研渲染器对齐哪一档要先拍板。

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
- 最终光照 = `brdfData.diffuse * lightColor * NoL + specularTerm * brdfData.specular * lightColor * NoL`——diffuse 项就是 Lambert（纯漫反射，无 Disney 漫反射），没有 rims/复杂项。

**HDRP 对比**：HDRP 的 Lit 走完整 GGX D、Smith 高度相关可见性 V、Schlick 菲涅尔 F（Schlick 是菲涅尔的常用近似公式），并按材料类型（标准/布料/清漆）换 BRDF；这里以 URP 为锚点，写自研渲染器时若追求质量可直接上完整项，差距主要在中高粗糙度的高光形状与强度。

## 4. 间接光（IBL）

URP 的 IBL 镜面项用解析近似，不用 2D BRDF LUT（预积分好的二维查找表）：specular 环境项 = (1/(α²+1)) × lerp(F0, F0 放大到掠射角, fresnel)，配合预滤波 cubemap（六面立方体贴图）的 mip（同一贴图逐级减半的缩小版本）——这是 split-sum（把环境镜面反射拆成"预滤波环境贴图"和"BRDF 积分项"两半再相乘的近似）的简化版。

来源：[GlobalIllumination.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/GlobalIllumination.hlsl)——IBL、探针混合、盒投影、SH 与 lightmap 的逐行核对对象。

URP 的环境项 = 漫反射 + 镜面（预滤波 cubemap + 解析 BRDF 项），即 split-sum 思想，但两半都取了便宜的近似。先解释三个词：SH 是球谐系数，用少量系数压缩环境光的方向分布；探针是场景里预置的光照采样点；光贴图是烘焙好的光照纹理。

```hlsl
// 镜面环境项（EnvironmentBRDFSpecular）：
surfaceReduction = 1.0 / (α² + 1.0)
specularEnv = surfaceReduction * lerp(F0, grazingTerm, fresnelTerm)
```

- 对照 Karis 的 split-sum（2D LUT 积分微面元 BRDF），URP 用的是**解析拟合**：粗糙度只进 surfaceReduction 一个标量，菲涅尔用 grazingTerm 做掠射放大。省一张 LUT 和一次采样，代价是各向同性近似、边缘处比 LUT 略偏。
- **mip 选择**：`mip = PerceptualRoughnessToMipmapLevel(perceptualRoughness)`，标准曲线 `perceptual * (1.7 - 0.7*perceptual) * levelCount`（core 的 ImageBasedLighting.hlsl）——注意曲线输入是 perceptual roughness 而不是 α。
- **反射探针系统**（`CalculateIrradianceFromReflectionProbes`）：
  - 每对象支持两个 cubemap（unity_SpecCube0/1）按**包围盒权重**混合：weight = 各轴到盒面距离的最小值 saturate；主导探针（importance 更高或体积更小；importance 指探针的重要性权重）会压缩另一方的权重，总权重不足 0.99 的余量交给全局环境 cubemap。
  - **盒投影**（BoxProjectedCubemapDirection）：反射向量与盒面求交把采样方向拉回盒内空间，修正"无限远 cubemap 在室内穿帮"。
  - GPU Resident Drawer（Unity 的 GPU 驱动渲染实现）路径下 lightmap 走**纹理数组**（unity_Lightmaps + unity_LightmapIndex 间接），否则走单张纹理——两种绑定并存，数组路线减少合批被打断但在部分 GPU 上不友好，这正是我们 bindless 表（把大量资源放进一张描述符数组、shader 用索引取）可以全吃掉的分支。
- **SSR**：`GlossyEnvironmentReflection` 里 SSR 结果按 alpha 与探针项 lerp，SSR（屏幕空间反射，用当前帧屏幕像素近似反射）未覆盖回退探针。

## 5. 环境漫反射：SH 与光贴图

同一文件里能看到 URP 的漫反射环境四个来源按宏分派：

1. **SH 探针**：`SampleSHVertex/SampleSHPixel`——L2 逐顶点算、L0+L1 逐像素算（EVALUATE_SH_MIXED），或全逐像素；unity_SHAr/Ag/Ab/Br/Bg/Bb/C 七个寄存器。L0/L1/L2 是球谐的阶数，阶数越高方向细节越多。
2. **光贴图**：`SampleSingleLightmap`（非方向）/ `SampleDirectionalLightmap`（方向光贴图，lightmap + 存法线分布的 indirection 贴图配对采样）。GPU-driven（GPU 驱动渲染）模式下是纹理数组。
3. **APV 探针体**：`EvaluateAdaptiveProbeVolume`，逐像素采样，可带 probe occlusion（探针遮挡，见谱系篇）。APV 是 Unity 的自适应探针体，用一组可细分的探针代替整张 lightmap。
4. **屏幕空间辐照度**（`_SCREEN_SPACE_IRRADIANCE`）：HDRP 式 SSGI（屏幕空间全局光照）输出的屏幕空间间接光直接 LOAD（直接读取像素，不走采样器）——不透明表面用之，透明回退环境探针。

## 6. 光照循环与衰减

来源：[RealtimeLights.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/RealtimeLights.hlsl)——距离衰减、聚光衰减与 shadow/subtractive 共存的逐行核对对象。

- **主光 + 附加光**：主光单独（实时阴影走阴影贴图），附加光逐像素循环（可选用结构化 buffer `_AdditionalLightsBuffer`，即 clustering/forward+ 路径——先按屏幕区块分光源、再逐物体着色的前向渲染）。
- **距离衰减**（与 Unity lightmapper 一致，平滑到 range 归零）：

```hlsl
lightAtten   = rcp(distanceSqr)
smoothFactor = saturate(1 - (distanceSqr * x)²); smoothFactor²    // x = 1/range²，预烘焙进 half2
return lightAtten * smoothFactor
```

其中 `rcp` 是取倒数，`saturate` 是钳到 [0,1]。

- **聚光角衰减**：线性插值 (SdotL − cosOuter)/(cosInner − cosOuter) 重排为 MAD（SdotL·a + b），**平方**后输出——内锥全亮、外锥平滑到零。
- **ShadowMask / subtractive**：两种让烘焙光照与实时阴影共存的模式。`SubtractDirectMainLightFromLightmap` 把主光的 Lambert 贡献乘 (1−shadowAttenuation) 从烘焙 GI（全局光照）里**减回去**，再 clamp 到 _SubtractiveShadowColor，最后取原值与减后值的较暗者——这就是"烘焙含太阳的 lightmap + 实时阴影"能在同一物体上并存的机制。

## 7. 能量守恒与多重散射

能量守恒是结构性的（diffuse×(1−reflectivity)）：URP 直接光里 diffuse/specular 的能量分配靠 `(1−reflectivity)` 一步去除，但没有 specular 与 diffuse 之间的二阶能量守恒（高光强时漫反射不会相应变暗），也**没有多重散射补偿**：metallic 高、smoothness 高时，低粗糙度金属会暗于物理正确值。

- Kajiya 的做法可作参照：反射走 VNDF 采样（可见法线分布采样，只采能被看到的微表面方向），VNDF 生成的方向无效（指进表面）时**重掷**（多重散射的射线分布仍近似原 BRDF 形状，Heitz 等 2016），能量守恒由末端预积分项补，并额外补金属多重散射带来的饱和度上升。
- 验证手段：**白炉测试**（纯白环境 + 纯白材质，不同 roughness/metallic 下画面应保持不变且不超曝）——自研渲染器上 IBL 管线的第一道验收。

## 8. 对 ash_renderer 的落地清单

1. **材质模型对齐 URP metallic 工作流**：albedo(sRGB→线性)、metallic、smoothness、occlusion（环境光遮蔽）四通道，材质数据所在的环形缓冲（项目里叫材质环）里已有的布局可直接扩展；F0=lerp(0.04, albedo, metallic) 一行。
2. **直接光 BRDF 建议直接上完整 GGX + Smith 高度相关项 + Schlick 菲涅尔**（WGSL 下无 half 精度压力，URP 的近似是给移动 half 逼出来的；完整项公式量级相同）。
3. **IBL 基建三样**：预滤波环境 cubemap（roughness→mip，GGX 重要性采样预卷积）、BRDF 积分项（选择：先抄 URP 解析近似零基建起步，后换 2D LUT 提质量）、mip 曲线按 perceptual 算。
4. **sRGB 线性域纪律**：albedo/env 输入 sRGB 解码，光照全部线性，输出一次编码（与库内 swapchain UNORM+SRGB view 的既定方向一致）。
5. **探针结构预留**：SH 环境漫反射（L0L1）是所有 GI 谱系篇方案的公共查询接口——先实现"从一组 SH 探针插值漫反射"的着色路径，GI 数据后面换（lightmap→APV→动态缓存）着色层不用动。

## 来源清单

- [URP BRDF.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/BRDF.hlsl)（直接光/预计算/金属工作流，逐行核对）
- [URP GlobalIllumination.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/GlobalIllumination.hlsl)（IBL/探针混合/盒投影/SH/lightmap）
- [URP RealtimeLights.hlsl](https://raw.githubusercontent.com/Unity-Technologies/Graphics/master/Packages/com.unity.render-pipelines.universal/ShaderLibrary/RealtimeLights.hlsl)（衰减函数/光照抽象）
- [kajiya GI overview](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)（多重散射 VNDF 重掷与预积分补偿）
- "Optimizing PBR for Mobile"（ARM，SIGGRAPH 2015 Mobile Course）——URP 注释中注明的 V×F 近似出处
