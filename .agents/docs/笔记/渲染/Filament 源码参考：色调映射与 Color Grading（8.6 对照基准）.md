# Filament 源码参考：色调映射与 Color Grading（8.6 对照基准）

本篇是 8.6 追加段（还原 Filament 完整 ACES 色调映射）的开工前置源码参考篇：把 Filament 成像链"颜色质量"的机制与常量逐处钉明，供 8.6 段施工计划与施工记录按号引用；本篇只做源码事实与对照差距，不主张施工方案细节（归 8.6 段施工计划）与 grading 全链的立案拍板。

出处锚点：本地检出 `F:\okzkx\filament` @ `373190d49`（rc/1.78.1-cut 开发线，2026-10-22 登记，与 8.2 BRDF 参考篇同一检出）。本篇产出于 2026-10-23，起因 = Sponza 同机位 unlit（饱和）与 shaded（发灰）对照实测后，用户拍板"还原 Filament"。

## 一句话结论

Filament 保住颜色靠三层：**①曲线本身**（完整版 ACES 拟合，AP0/AP1 色域内工作 + glow 模块 + 红色修正 + 受控两级去饱和——viewer 默认的 ACES_LEGACY 也走这套，只是加 1.667 亮度增益）、**②EVILS 亮度缩放**（可选开关：只对亮度做 tone map，色度比例原样保留）、**③整条 color grading 烘 32³ 3D LUT**（白平衡/CDL/对比度/vibrance/saturation/曲线全链，saturate 与 OETF 只在最后）。本渲染器现行链只有裸 Narkowicz 逐通道拟合 + 曝光钉在其去饱和最猛的肩部，是"发灰"观感的主源。

## 现状定位（ash_renderer 侧证据）

- 现行曲线：`ash_renderer/shaders/present_tonemap.wgsl:50` 的 `(x(2.51x+0.03))/(x(2.43x+0.59)+0.14)`（Narkowicz 2015 拟合，Rec.709 逐通道）。这正是 Filament 注释里的 "legacy fit of ACES in Rec.709"（`filament/src/ToneMapper.cpp:173-174`）——Filament 自己只把它当亮度对照基准，不在正式链路裸用。
- 曝光落点：`ash_renderer/shaders/gbuffer_composite.wgsl:121` `EXPOSURE = 0.00032`，锚点注释"白墙 ≈ 1.7 HDR 进肩部不截断"（8.4.1 重校准，治"全顶白"）——受光面大面积落在曲线肩部。
- 发灰实测（2026-10-23，Sponza 同机位 BRP 三连取证）：unlit 直出反照率饱和度约 23%（暖沙色，贴图不灰）；点亮后受光面 (221,220,219) 近中性灰白，饱和度约 1%；BRP 关 ACES 直出后饱和度只回升到约 14%——说明曲线选型是次因，曝光落点与高光压缩才是主因。

## 二、Filament 色调映射算子家族

viewer 侧枚举（`libs/viewer/include/viewer/Settings.h:67-77`）：LINEAR / ACES_LEGACY / ACES / FILMIC / AGX / GENERIC / PBR_NEUTRAL / GT7 / DISPLAY_RANGE 九种；工厂 `libs/viewer/src/Settings.cpp:994-1011`。viewer 默认 `toneMapping = ACES_LEGACY`（Settings.h:163）。

关键事实：**ACESLegacyToneMapper 不是裸拟合**——它调同一个完整版 `aces::ACES(c, 1.0f/0.6f)`（`ToneMapper.cpp:391-393`，亮度增益 1.667）；完整版 ACESToneMapper 只是 `aces::ACES(c, 1.0f)`（`:379-380`）。即 Filament 两条 ACES 路线都经过全拟合管线，差别只在亮度增益位。

### 完整版 aces::ACES 实现清单（ToneMapper.cpp:133-193，8.6 照抄对象）

执行序与常量（全部实测核实于该检出）：

1. **色域进**：`ap0 = Rec2020_to_AP0 * color`（`:150`；矩阵常量在 `ToneMapper.cpp` 顶部，施工时逐值照抄）。
2. **Glow 模块**（`:152-157`）：`rgb_2_saturation`（`:42`）、`rgb_2_yc`（ycRadiusWeight=1.75，`:51`）、`sigmoid_shaper`（`:76`）、`glow_fwd`（`:83`）；常量 `RRT_GLOW_GAIN=0.05`、`RRT_GLOW_MID=0.08`——按饱和度加低强度光晕，暗部不发死。
3. **红色修正**（`:159-165`）：`rgb_2_hue`（`:97`）+ `center_hue`（`:110`）；常量 `RRT_RED_SCALE=0.82`、`RRT_RED_PIVOT=0.03`、`RRT_RED_HUE=0`、`RRT_RED_WIDTH=135`——保护红色相不被压脏。
4. **AP0→AP1** 并 clamp 到 half 上界（`:168`）。
5. **一级受控去饱和**：`ap1 = mix(luma, ap1, RRT_SAT_FACTOR=0.96)`（`:171`）。
6. **亮度增益位**：`ap1 *= brightness`（`:175`；ACES=1.0 / ACES_LEGACY=1.667——本渲染器的曝光/亮度旋钮应挂这一位）。
7. **Hill RRT+ODT 拟合**（`:177-184`，出处 colour-unity CIECAM02 notebook）：`(x(ax+b))/(x(cx+d)+e)`，常量 `a=2.785085, b=0.107772, c=2.936045, d=0.887122, e=0.806889`——与 Narkowicz 同形状不同常量，但拟合对象是完整 RRT+ODT。
8. **暗包围补偿**：`darkSurround_to_dimSurround`（`:120-131`，DIM_SURROUND_GAMMA=0.9811，经 AP1→XYZ→xyY 幂变换→回 AP1）。
9. **二级受控去饱和**：`mix(luma, linearCV, ODT_SAT_FACTOR=0.93)`（`:190`）。
10. **色域出**：`AP1_to_Rec2020 * linearCV`（`:192`）。

与裸 Narkowicz 的质的差别：逐通道拟合 RGB 三通道先后进肩部 → 色相偏移 + 高光洗白；全拟合在工作色域里做 glow/红修正/两级去饱和，肩部滚降受控。

## 三、EVILS 亮度缩放（可选层，未立案）

`filament/src/details/ColorGrading.cpp:591-618`（Troy Sobotka 2021, "EVILS - Exposure Value Invariant Luminance Scaling"）：`luminanceIn = dot(x, w)` → `luminanceOut = toneMapper(luminanceIn)` → 色度比例 `chromaRatio = x/peak` → 按 `maxReserves = 1-chromaRatio` 把亮度增量按比例摊回——**只滚降亮度，色度比例分毫不动**。viewer 开关 `luminanceScaling`（默认 false，Settings.h:159）。

同族可选 `gamutMapping`（色域外兜底，`ColorGrading.cpp` 中 `gamutMapping_sRGB`）。

## 四、Color Grading LUT 管线（第三层，未立案）

Filament 把整条 grading 预烘焙进 32³ 3D LUT（QualityLevel 控维度），逐像素成本 = 一次 3D 纹理采样。逐样本管线顺序（`ColorGrading.cpp:1020-1080`，8.6 不主张、留后续拍板）：

夜适应（scotopicAdaptation，Cao et al.）→ 白平衡（CIECAT16 von Kries，`:459`）→ 通道混合 → 阴影/中间调/高光分级（tonalRanges）→ **转 LogC** → ASC CDL（S-2016-001）→ 对比度（Resolve 同式，锚 = `MIDDLE_GRAY_ACEScct`）→ **回线性** → vibrance（按饱和度自适应）→ saturation → RGB 曲线 → **色调映射（含 EVILS 分支）** → 输出色彩空间变换 → gamutMapping → saturate → OETF（sRGB 编码）→ custom LUT。

要点：**saturate 与 OETF 只出现在最后一步**；LUT 烘焙是性能手段而非质量前提（全屏 pass 直接算矩阵成本可接受，8.6 不做 LUT 化）。

## 五、曝光口径

- viewer 默认 `exposure = 0.0`（EV 刻度，2^EV 乘在 grading 链最前，Settings.h:203）——Filament 不用曝光救曲线，场景标定靠光度单位（方向光 lux + IBL）。
- 对比度锚 `MIDDLE_GRAY_ACEScct`（`ColorGrading.cpp:838` contrastOffset 基准）——ACEScct 域中灰，非我们线性域 0.18 直搬。
- 我们侧对应物：`EXPOSURE = 0.00032` 常量 + 20000 lx 满日照 + 800 lx ambient。8.6 需重校曝光锚点（典型受光面落线性段 ~0.5–0.8 而非肩部 1.7），并把亮度增益位与 Hill 拟合的量纲分开。

## 六、PBR_NEUTRAL 形状与决策

Khronos PBR Neutral（`ToneMapper.cpp:446-464`，出处 modelviewer.dev tone-mapping 示例）：亮度 0.8 以下完全不动；高光段按峰值压缩到 1.0（startCompression=0.8-0.04=0.76），仅配 desaturation=0.15 的轻微去饱和，色相全程不动——设计目标即"产品渲染中饱和色不许变灰变相"。

**决策（2026-10-23，AI 按用户授权自决）：不做。** 理由：还原口径 = Filament 现役成像链，PBR Neutral 是 Khronos 方案不在 Filament 链内；完整 ACES + 曝光锚点重校已覆盖其收益面（高饱和内容高光保色）。恢复条件：8.6 收官后高饱和内容高光褪色仍不满意时——成本约 10 行 WGSL，可挂现有 `bloom_set` 开关做三态 A/B，随时可加。

## 七、对照差距表

| 层 | Filament | ash_renderer 现状 | 8.6 主张 |
|---|---|---|---|
| 曲线 | 全拟合 ACES（AP0/AP1 + glow + 红修正 + Hill 拟合 + dim surround），LEGACY=同管线+1.667 增益 | 裸 Narkowicz 逐通道（Rec.709） | ✅ 替换为完整版，亮度增益位外露 |
| 曝光 | EV 旋钮 + 光度单位标定，ACEScct 中灰锚 | EXPOSURE 常量，白墙钉在 1.7 HDR 肩部 | ✅ 锚点重校（受光面落线性段），随曲线替换重标 |
| 亮度/色度分离 | EVILS luminanceScaling + gamutMapping（开关，默认关） | 无 | ⬜ 不进 8.6；若 8.6 后高光仍褪色再立案 |
| grading 全链 | 白平衡/CDL/vibrance/saturation/曲线烘 32³ LUT | 无 | ⬜ 不进 8.6，留拍板（步骤 9 IBL 后按需） |
| 间接光 | IBL 提供有色环境光 | 800 lx 灰 ambient（4%） | ⬜ 归步骤 9.1 间接光注入 |
| 编码纪律 | grading LUT 末尾 OETF 一次 | swapchain SRGB view ROP 恰好一次（3.4 拍板） | 不变，8.6 置于同纪律下 |

## 八、发灰问题归因（8.6 判定线的现象基线）

三因子按贡献排序：①曝光锚点把受光面送进曲线肩部（去饱和主源）；②ACES 家族曲线本身的高光压缩（换曲线可显著缓解但不归零）；③暗部只剩灰 ambient 无色相、中间调被 filmic 压缩（归步骤 9）。8.6 消①②，③留步骤 9——判定线须包含"同机位 unlit vs shading 饱和度差"与"高光段饱和色读图"两类对照，防只修曲线不修锚点。
