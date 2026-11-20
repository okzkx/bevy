# SCGI 的间接光注入端：谁的环境光被替换

9.2 的 IBL 做完之后，环境光已经不再是一个常量色；但它也不是"所有材质统一换成 SCGI"。准确的替换面要分两层看：**IBL 是所有受光材质的环境基线；SCGI 只替换不透明像素、Volume 内的 IBL 漫反射项。** 这篇把两层的公式、覆盖范围和数据来源演进钉清楚；Unity 侧的缓存结构见[Unity SCGI 实现](Unity%20SCGI%20实现.md)，本工程 9.2 的施工口径见[施工计划：天空盒IBL](../../开发步骤/9-表面缓存全局光照/9.2-天空盒IBL/施工计划：天空盒IBL.md)，SCGI 的 9.7 注入见[施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../../开发步骤/9-表面缓存全局光照/施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)。

## 一句话

**IBL 开：所有受光材质的 `albedo × ambient` 换成天空 SH-L2 漫反射 + 预滤波高光；SCGI 开：只把不透明像素、Volume 内的 IBL 漫反射换成缓存 `E(P,N)`；透明层、背景、调试模式不接 SCGI；IBL 关回步骤 8，SCGI 关回 IBL 基线。**

## 1. 两层：IBL 是基线，SCGI 是增量

| 层 | 数据来源 | 覆盖 | 开关 | 关掉之后 |
|---|---|---|---|---|
| IBL（9.2） | 天空 cubemap → SH-L2（漫反射）+ 预滤波 cubemap/DFG LUT（高光） | 所有受光材质：不透明 + 透明 | `ibl_enabled` | 回到步骤 8 的 `albedo × ambient_color` |
| SCGI（9.7） | Volume/cell/patch 的世界空间缓存 `E(P,N)` | 只覆盖不透明像素、Volume 内 | `gi_enabled` | 回到 IBL 基线 |
| 可见天空（9.1） | `sky_radiance(dir)` | 背景像素 | `sky_enabled` | 回到清屏色 |

- 背景天空与 IBL 正交：可以有天空没 IBL（对照），也可以有 IBL 没可见天空（只测材质）。
- 透明层接 IBL，因为 IBL 是新的环境基线；SCGI 不接透明，因为 SCGI 只有不透明 G-buffer 的位置/法线/材质可取。
- 每层开关都必须能逐位回退到上一层；这是验收硬线，不是"看着差不多"。

## 2. 注入公式

### 2.1 IBL（不透明 + 透明）

```
ibl_diffuse  = diffuseColor · Fd_Lambert() · E(n) · ibl_intensity
ibl_specular = prefiltered(R, perceptualRoughness) · (F0 · dfg.x + dfg.y) · ibl_intensity
shaded       = light_sum + ibl_diffuse + ibl_specular
```

- `E(n)` 是天空 SH-L2 的辐照度，已含余弦卷积；`Fd_Lambert() = 1/π`，与直接光漫反射同口径。
- `diffuseColor = albedo · (1 − metallic)`；金属只有高光，没有 IBL 漫反射。
- `prefiltered` 是 GGX 预滤波 cubemap，`R = reflect(-V, n)`；`dfg` 是 128² 的 DFG LUT（`x = A`、`y = B`）。
- 太阳圆盘不在 cubemap 里；太阳的直射与高光仍由直接光 `surfaceShading` 负责，避免双计。
- 天空/IBL 与直接光同域（pre-exposure），合成 pass 末尾统一乘 `EXPOSURE`。

### 2.2 SCGI（只替换 Volume 内的不透明漫反射）

```
// Volume 内、不透明
indirect = pixel.diffuseColor · Fd_Lambert() · E_cache(P, N) · gi_intensity
shaded   = light_sum + indirect + ibl_specular        // 高光仍是 9.2 的 IBL

// Volume 外 / 透明
shaded   = light_sum + ibl_diffuse + ibl_specular     // 回退 IBL
```

- SCGI 只换漫反射，不碰高光；镜面间接（SSR/局部反射探针）不在步骤 9。
- Volume 外没有 patch 时回退 9.2 的 SH-L2 或 9.3 的 GlobalProbe（按 9.3 的 A/B 结论）。
- `gi_enabled=false` 时原样走 IBL；`ibl_enabled=false` 时原样走步骤 8 的 `albedo × ambient_color`。

## 3. 覆盖范围

![环境光项的替换范围：IBL 覆盖不透明与透明；SCGI 只覆盖不透明 Volume 内的漫反射；背景与调试模式不接](../_assets/scgi-ambient-replacement-scope.png)

| 路径 | IBL 开 | SCGI 开 | 原因 |
|---|---|---|---|
| 不透明、Volume 内 | SH-L2 diffuse + 预滤波高光 | 漫反射换成缓存，高光仍 IBL | 唯一同时有 G-buffer 位置/法线/材质与缓存的路径 |
| 不透明、Volume 外 | SH-L2 diffuse + 预滤波高光 | 回退 IBL（或 9.3 GlobalProbe） | 没有 cell/patch 可查 |
| 透明（forward） | SH-L2 diffuse + 预滤波高光 | 不接 | 透明不写 G-buffer，也没有缓存查询的位置 |
| 背景 / 清屏 | 9.1 天空（`sky_enabled`） | 不接 | 无表面 |
| 调试模式 | 无光照直出 | 不接 | 调试仪器旁路 |

所以"所有材质"要拆成"所有受光材质（IBL）"与"不透明 Volume 内（SCGI）"；透明层只属于前者。

## 4. "有方向性"不等于"每个材质不同值"

IBL 在空间上仍是一个全局环境，但它不是常量：

- **求值用每个像素的法线**：同一套 SH-L2 系数，朝上的地板与朝下的天花板拿到不同的 E(n)；
- **高光用反射方向与粗糙度**：金属/光滑面看到天空的不同区域；
- **再乘各自的 diffuseColor/F0**：不同材质的环境项仍随 albedo/metallic/roughness 变化；
- **金属不接漫反射间接**：metallic = 1 时 `diffuseColor = 0`；
- **AO 暂不乘 IBL**：与旧环境项同口径，9.8 局限对照时再裁决；
- **开关默认值**：9.2 段内 `ibl_enabled` 默认关（BRP 开），基线仍与步骤 8 一致；9.8 验收后再拍板是否默认开。IBL 开时始终有天空参数，不存在"没设天空"的状态。

SCGI 再进一步：它不仅随法线变，还随**位置**变（Volume 内逐 cell/patch），但只作用于不透明漫反射。

## 5. 数据来源的演进

| 阶段 | 不透明漫反射 E | 高光 | 空间变化 | 成本 |
|---|---|---|---|---|
| 9.1 | 无（仍是 `albedo × ambient`） | 无 | 无 | 天空背景，一个全屏分支 |
| 9.2 | 天空 cubemap → SH-L2 | 预滤波 cubemap + DFG LUT | 无（只随法线） | 捕获 + SH/预滤波，天空变化时重烘 |
| 9.3 | GlobalProbe 16×L1（A/B/兜底） | 同 9.2 | 无（只随法线） | +512 次 cubemap 采样，跟捕获同批 |
| 9.7 | Volume 内：cell/patch 缓存 `E(P,N)`；外：IBL/GlobalProbe | 同 9.2 | 有（逐 patch） | 随 patch 数/分辨率，不随实例数 |

9.3 的 16 桶不是"比 L2 更准"的自动升级；它是方向类别 LUT 与 patch 兜底格式，默认模式由 A/B 决定。详见[SH 与光照探针](SH与光照探针.md)的"16 桶 vs 单个 L2/L3"。

## 6. 概念边界：Cell/patch 与 GlobalProbe 是两套东西

- **Cell/patch**：体积内的世界空间缓存；每个体素 16 个**表面类别槽** → 共享 patch 池；解决体积内逐位置/逐表面；9.5 起。
- **GlobalProbe**：一个全局环境按 16 个**法线类别**拆成的 LUT（16×RGBL1；Unity 768B，本工程按 WGSL 对齐 1KB）；全局一份、没有位置变化；只作体积外 `DistanceFallback` 或 9.3 的 A/B 模式。

两者的"16"都来自同一套 4×4 八面体桶，但一个是逐体素的表面类别槽，一个是全局的法线类别条目。图和详细对照见[SH 与光照探针](SH与光照探针.md)。

## 7. 本工程锚点

- 现状（步骤 8）：`shaders/scene/gbuffer/gbuffer_composite.wgsl` 的环境项、`shaders/scene/transparent/transparent_forward.wgsl` 的透明环境项；
- 9.1：`shaders/common/sky.wgsl`、合成 pass 背景分支、帧 UBO 的天空参数；
- 9.2：`shaders/common/ibl.wgsl`、`shaders/scene/ibl/`、`src/render/passes/ibl/`、IBL 描述符集；以[9.2 施工计划](../../开发步骤/9-表面缓存全局光照/9.2-天空盒IBL/施工计划：天空盒IBL.md)为准；
- 9.3：`shaders/common/octahedral.wgsl`、`shaders/scene/ibl/global_probe.wgsl`；以[9.3 施工计划](../../开发步骤/9-表面缓存全局光照/9.3-GlobalProbe方向桶/施工计划：GlobalProbe方向桶.md)为准；
- 9.7：屏幕 gather 与 Volume 内注入；以[八段拆解 §2 9.7](../../开发步骤/9-表面缓存全局光照/施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)为准。
