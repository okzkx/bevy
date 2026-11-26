# 施工计划：9.7 屏幕 gather 与最终注入

> 2026-11-24 立案（纯文档，未开工）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.7](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.7 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录（`9.7.K-*.md`），不回写本文。注入公式与覆盖范围见[SCGI 间接光注入端](../../../笔记/光照与材质/SCGI间接光注入端.md)§2、§3；屏幕侧四级滤波的 Unity 依据见 [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)§"收敛怎么做的"、§"屏幕端怎么消费"。
>
> **当前状态：✅ 主体收官（2026-11-24，分支 `goal/9.7`）。** 七篇施工记录 + 流程导读齐；判定线 ①②③④⑥⑧⑨ 闭环、⑤⑦ 部分（块状已调档留图，Volume 边界环与漏光案例未专项留档）；视图 16/17 与 `scgi_ms` 计时点位按 §5 待决顺延。下面这段是开工时的原话，保留作沿革：施工中（2026-11-24 开工，起点 `a331d092b`，[段 README](README.md) 是施工面板）。 任务 9.7.1～9.7.7 为待施工拆解；前置闸门见 §3、判定线草案见文末。一次只施工一段：按任务号推进，任务内实现、验证、记录后再进下一任务。

**一句话说明**：每帧在 1/4 分辨率把像素反投影成世界位置、查 Volume 内 rank 0 的 patch，先做 patch 级空间/时域滤波，再把查表结果压成 L0+L1 去噪并做全分辨率双边上采样，最后由合成 pass 用这张全分辨率辐照度替换 Volume 内不透明像素的 IBL 漫反射——这是"缓存被着色消费"的唯一入口，也是 9.8 动态验收与压测要读的那张画面；本段不做动态收敛验证与压测（归 9.8），不改射线侧与分配侧，不碰透明层与高光。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-24 现状核实清单（本 checkout，逐项有出处）

| 项 | 结论 | 依据 |
|---|---|---|
| 注入点唯一 | 不透明环境项只有一处求值 `ibl_environmental()`，返回 `IblTerms{diffuse, specular}`；SCGI 只换 `diffuse`，高光原样 | `shaders/scene/gbuffer/gbuffer_composite.wgsl:89-125`、同文件 `:224` |
| 透明层同口径 | 透明前向自己求一遍 IBL（`sh_irradiance` / `probe16_eval` / `ibl_specular`），**不读**不透明合成结果 | `shaders/scene/transparent/transparent_forward.wgsl:83-99` |
| 求值函数现成 | `sh_irradiance(n, coeffs)` 与 `probe16_eval(n, l0, l1x, l1y, l1z)` 吃的都是"已折余弦卷积与基常数"的系数，patch 的 RGBL1 与它们同口径（求值即 `l0 + l1·n`） | `shaders/common/ibl.wgsl:65-85`、`shaders/common/gi_cell.wgsl:34-40` |
| 合成 pass 集数 | 合成只绑 5 集（frame / gbuffer / cluster / shadow / ibl），新增 SCGI 纹理要加第 6 集；`FullscreenPass` 的 `sets` 是定长切片 | `src/render/frame/record.rs:290-305` |
| 帧 UBO 无空位 | 336B 已排到 `sky_uniform`（320..336），`ibl_enabled/ibl_mode/ibl_intensity` 在 304/308/312；扩开关要新增 16B 槽并同步四处（布局镜像断言、WGSL `FrameUniforms`、打包函数、默认值） | `src/render/resident/tables/frame_uniform_table.rs:39`、`:141-158` |
| 1/4 网格口径已在 9.5 定死 | 播种的 1/4 pixel = `full_x = low_x*4 + sub%4`、`sub = (frame*7)%16`；gather 若按块心 `low*4+1.5` 单点取，查到的 patch 与本帧播种的子像素不是同一格，命中率会假低 | `shaders/scene/gi/patch_alloc.wgsl:87-98` |
| 查表链现成但带副作用 | `cache_evaluate()` 已把"级联归属→体素→存储槽→16 桶→cell→patch"整链写完，同时带 bounce 分配副作用；9.7 要**只读**版，须抽公共片段而不是复制 | `shaders/scene/gi/estimation.wgsl:90-140` |
| Volume 判据 | 球判据 `resolve_cascade(center, p, voxel_min, res, cascades)`，都不满足返回 `INVALID_PATCH`；宿主侧同一判据在 `volume.rs`，两侧有互证测试 | `shaders/common/gi_cell.wgsl:95-108`、`src/render/passes/gi/volume.rs:103-110` |
| 1/4 尺寸来源现成 | 播种的 push 常量已带 `low_w/low_h/full_w/full_h/frame_index`，gather 沿用同一份，不另建 | `shaders/scene/gi/patch_alloc.wgsl:40-52` |
| 计时点位 | `TimestampPoint` 只有 0..11，无 estimation 点位（9.6 §5 的待决实际裁决为并入 `SurfaceCache` 区间，`gi_ms` 覆盖 ④.5 整段） | `src/render/stats/perf.rs:48-73` |
| 调试图号 | GI 视图 0..13 已排满（0 关 / 1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 / 6 池水位 / 7 位置偏离 / 8 cell 归属 / 9 样本档 / 10 辐照度 / 11 方差 / 12 更新次数 / 13 rank 分布），屏幕侧视图从 14 起 | `src/remote/features/gi.rs:108-110` |
| 9.7 全部是新代码 | 在 `ash_renderer/src` 与 `ash_renderer/shaders` 检索 `screen_gather`、`spatial_filter`、`temporal_filter`、`upsample`、`gi_compression` 五个关键词零命中 | 2026-11-24 检索 |

### 0.1 口径订正一：屏幕端不是"逐像素查缓存"，是"1/4 查表 + 全分辨率双边上采样后按着色法线求值"

八段 §2 9.7 行写的"per-pixel gather：距离/法线一致/年龄渐入加权；SH L1 × per-pixel shaded normal"把两件事揉成了一件。Unity 的形状是：缓存查询只在 1/4 分辨率做（`ScreenResolveLookup` → 去噪 → `ScreenResolveUpsampling`），**全分辨率那一步只做双边上采样与 SH 求值**，不再查缓存（[Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)§"屏幕级：低分辨率查找、去噪、上采样"）。本段按 Unity 形状立案：查表成本 ∝ 分辨率/4²，全分辨率只付一次采样加一次 `l0 + l1·n`。路线图 9.7 行的措辞在本次立案同步时一并改成这个形状。

### 0.2 口径订正二：撤掉"Chebyshev 软可见性"

八段 9.7 行列的"Chebyshev 软可见性"在 Unity SCGI 里没有对应实现（Unity 屏幕查表用的是：中心样本权重 0.01 且不查 rank、抖动样本只收 rank 0、位置在格子尺寸内抖动、方向在 `cos = 0.9` 的锥内抖动）。Chebyshev 是 shadow-map 系软阴影的常用手段，凭空加进来会引入没有出处的滤波面。本段撤销这条，改用 Unity 同款的两条防漏光/防噪手段：**rank 0 过滤**（结构性）与**法线 + 平面距离权重**（数值性）。

### 0.3 开关：SCGI 自己一个开关，不动 `gi.enabled` 的语义

`gi.enabled`（9.5）今天是"表面缓存这套表生不生效"，关掉 = 零命令零屏障、**不改画面**（9.5 与 9.6 的硬线）。本段新增的注入要能 A/B，就必须独立开关：`scgi`（帧 UBO 新槽 `scgi.x`，0 关 / 1 开），语义 = "合成 pass 用不用 SCGI 辐照度"，与 `gi.enabled`（生产者）正交。四种组合都要能跑：`gi` 关 `scgi` 开 = 读不到缓存 → 全屏回 IBL（等价于关）；`gi` 开 `scgi` 关 = 只生产不消费（9.6 现状）。

### 0.4 中间目标的存储格式

| 目标 | 分辨率 | 格式 | 内容 |
|---|---|---|---|
| `scgi_l0` | 1/4 | `R16G16B16A16_SFLOAT` | rgb = L0 辐照度（**物理单位**），a = 覆盖权重（命中 rank 0 的样本权重占比） |
| `scgi_l1` | 1/4 | `R8G8B8A8_UNORM` | rgb = `l1 × √3/(4·l0) + 0.5` 编码，a 保留（可放抖动相位） |
| `scgi_irradiance` | 全分辨率 | `R16G16B16A16_SFLOAT` | rgb = 按全分辨率着色法线求值后的 `E(n)`，a = Volume 内覆盖率（0 = 不接管） |

- 1/4 两张中间目标双缓冲（去噪与 A/B 读上一帧），全分辨率那张单缓冲。
- 曝光：缓存与中间目标全部存**物理单位**，注入处与 IBL 同域、合成末尾统一乘 `EXPOSURE`（`shaders/common/brdf.wgsl:15` 的 `0.00016`；本工程固定曝光、无自动曝光，故不做 Unity 的 pre-exposure 还原）；只有调试图（判定线读图）才乘曝光。
- fp16 上限风险进 §5 待决（峰值辐亮度需首测）。

### 0.5 Volume 内外由 gather 侧判定，合成端只看掩码

合成分支不重算 `resolve_cascade`（那要把 GI UBO 与 cell 表一起绑进片元，白付带宽）：上采样 pass 在 Volume 外写 `a = 0`，合成端 `mix(ibl_diffuse, scgi_diffuse, a)`。代价是 Volume 边界有一圈 1/4 分辨率的软过渡（可接受，且正好盖住边界跳变），做法与代价都进判定线⑤读图。

### 0.6 patch 级两级滤波进本段（9.6 §0.8 的转交确认）

`SpatialFiltering`（切平面抖动 taps，权重 `1 − |jitter|/√2`，查不到的样本跳过）与 `TemporalFiltering`（`lerp(0.9, 0.95)`，首帧直取）都写 patch 表，归本段。9.6 已有的"累计平均 → 方差引导滞后"在 estimation 内，这两级是它之后的后处理，串行、可各自关闭做 A/B。

### 0.7 边界（本段不主张什么）

- **不做动态收敛验证与压测**：动态几何/动态光照/TOD 下的残影、Volume 边界兜底、成组压测、局限逐项自检全归 9.8。本段只保证"静态能看、开关能回退"。
- **不改射线侧**：样本档、预算、多跳、bounce 分配、光源清单、自发光三角形列表一律不动（9.6 已收官）；本段只读它们产出的 patch 表。
- **不接透明与高光**：透明层继续走 9.2 的 IBL（`transparent_forward.wgsl` 不改），高光仍是 9.2 的 split-sum；SSR/局部反射探针/镜面间接仍不主张。
- **不做 AO 与 lightmap**：AO 不乘 IBL/SCGI（与旧环境项同口径，9.8 再裁决）；不做烘焙、不做路径追踪。
- **不做运动矢量与 TAA**：屏幕侧时域只在 1/4 目标上用上一帧做指数混合（9.7.3 是 patch 空间），不做全屏重投影 TAA；全步边界"TAA 不做"不变。
- **不做多相机/多视口**：一份 Volume、一个主相机（9.5 已定），本段不扩。
- **不改 9.3 的默认模式**：Volume 外就是 IBL 基线（`frame.ibl_mode` 决定 SH-L2 还是 16 桶），不在此段重做 A/B。
- **透明与 Volume 外的"真 GI"另案**：透明不吃 SCGI、Volume 外只有无限远环境，这两条缺口若被 9.8 判为不可接受，补法是**位置相关的探针体积**或**屏幕空间步进**，已登记为[外置条目 18（APV）](../../外置拓展需求/外置拓展需求.md)与[外置条目 19（SSGI）](../../外置拓展需求/外置拓展需求.md)；本段不得顺手实现其中任一条。

## 1. 数据流总览

每帧在 ④.5 段（播种与 estimation 之后、合成之前）追加 5 个 dispatch + 1 个可选视图：

| 步 | dispatch | 输入 | 输出 |
|---|---|---|---|
| ① 空间滤波 | 池容量个线程 | patch 几何/统计/本帧辐照度、cell 表、GI UBO | 滤波后辐照度（写 `filtered` 缓冲 A） |
| ② 时域后滤波 | 池容量个线程 | ① 的 A、上一帧的 B、GI UBO | 写 B（首帧直取 A） |
| ③ 1/4 查表 | `(low_w/8)×(low_h/8)` 组 | ② 的 B、patch 统计（rank 0 过滤）、G-buffer normal + depth、GI UBO、push（两档尺寸 + 帧号） | `scgi_l0` / `scgi_l1`（1/4，写当前缓冲） |
| ④ 去噪 | `(low_w/8)×(low_h/8)` 组 | ③ 当前缓冲、G-buffer normal + depth | 覆盖写 ③ 的缓冲（0～N 遍 3×3，步长 `{1,2,2,3}`） |
| ⑤ 双边上采样 | `(full_w/8)×(full_h/8)` 组 | ④ 结果、全分辨率 G-buffer normal + depth | `scgi_irradiance`（全分辨率 RGBA16F，a = 覆盖率） |
| ⑥ 视图（可选） | 全分辨率或 1/4 | 上述任意缓冲 | 屏幕侧调试图（视图 14 起） |

- 合成 pass 读 `scgi_irradiance`（第 6 集）：`scgi.x = 1` 且 `a > 0` 时替换漫反射。
- 屏障：② → ③（compute→compute）、⑤ → 合成（compute→fragment，沿用 `GiFrameDraw::record` 段末那道屏障的形状并扩到新贴图）；写读之间不插 `device_wait_idle`（统一验收口径第 6 条）。
- 顺序理由：① ② 在 patch 空间（世界），③ ④ ⑤ 在屏幕空间；前者必须先跑，否则屏幕读到未滤波值。

## 2. 任务拆解

### 9.7.1 只读查表基座：`lookup_patch(p, n)` 与 1/4 网格映射

**做什么**

- 把 `estimation.wgsl:90-140` 的查表链（`resolve_cascade` → `voxel_size` → `volume_position` → `volume_to_storage` → `direction_to_bucket_index` → `cell_index` → `cell_patch`）抽成 `shaders/common/gi_lookup.wgsl` 的只读函数，返回 `{hit: bool, patch: u32, cascade: u32, rank: u32}`；estimation 内的 `cache_evaluate` 改为调用它再叠自己的 bounce 分配副作用（行为逐位不变）。
- 给出与播种同源的 1/4 网格映射：`sub = (frame*7)%16`、`full = low*4 + sub%4 / sub/4`；查表用的 1/4 样本点取该 4×4 块的**对角两点 + 块心**（默认 3 点，可调），并在注释里写清"不按块心单点取，避免与本帧播种子像素错格"。

**落点**：`shaders/common/gi_lookup.wgsl`（新）、`shaders/scene/gi/estimation.wgsl`（改调用）、`src/render/passes/gi/mod.rs`（片段注册）。

**坑 / 钉子**

- 抽取不许改 estimation 的字节输出：`estimation=true` 前后逐位一致（用 `gi_dump` 同通道同帧槽比，跨进程 PPM 会假失败）。
- `resolve_cascade` 的 `center` 来自 GI UBO `target_pos`，不是帧 UBO 的 `camera_pos`——今天同值，抽取时别顺手换源。
- 本段函数**只读**、不带 bounce 分配：rank 1 的 patch 能被查到，但过滤在 ③ 做，函数只如实回 rank。

### 9.7.2 patch 级空间滤波（切平面抖动 taps）

**做什么**

- 每活跃 patch 在切平面按 `spatial_radius × 体素边长` 投 N 个抖动样本（默认 4），每个样本用 9.7.1 的只读查表取邻居辐照度，权重 `1 − |jitter|/√2`，归一化后写 `filtered` 缓冲 A；**查不到邻居的样本跳过**，不强行混合。
- 只处理 `rank 0` 且 `update_count ≥ 1` 的 patch（rank 1 与刚分配的不参与，避免把 IBL 种子抹开）。
- 开关 `spatial_filter`（`GiSettings` + BRP + 面板）；关 = 直拷 estimation 结果（A 与源逐位一致）。

**落点**：`shaders/scene/gi/spatial_filter.wgsl`（新）、`src/render/passes/gi/filtering.rs`（新）、`src/render/resident/tables/surface_cache.rs`（`filtered` 辐照度表与绑定登记）。

**坑 / 钉子**

- 只读本帧 estimation 的输出、写另一份 `filtered`，**不原地写**：同一 dispatch 内边写边读邻居，读到的值取决于调度，等于引入不可复现噪声。
- 切平面基要按 patch 法线建正交基（`shaders/common/ibl.wgsl:86` 的 `tangent_frame` 可复用）；世界缓存按着色法线建桶，滤波也按它，别用 G-buffer 平面法线。
- 显存：`filtered` ×2 份 × 65536 × 64B = 8 MiB（进 §4）。

### 9.7.3 patch 级时域后滤波（`lerp(0.9, 0.95)`）

**做什么**

- 对 9.7.2 的 A 与上一帧 B 做指数混合：`w = lerp(0.9, 0.95, temporal_smoothing)`、`B = lerp(A, B_prev, w)`；`update_count == 0`（或首次出现）的 patch 直取 A。
- 开关 `temporal_filter`；关 = A 直通 B。

**落点**：`shaders/scene/gi/temporal_filter.wgsl`（新）、`src/render/passes/gi/filtering.rs`、`src/remote/features/gi.rs`（新参数 `temporal_smoothing`）、`src/overlay/tool_window/gi.rs`。

**坑 / 钉子**

- 双缓冲奇偶与 estimation 的 ping-pong 是**两套**：estimation 帧奇偶决定"写哪份辐照度"，滤波 A/B 由自己的帧奇偶决定，别复用同一索引（写错会读到"上一帧的上一帧"）。
- 本段 1/4 查表**不做**重投影（只做空间去噪）；`prev_view_proj` 仍只服务 9.6 的种子重投影，不要以为它会给屏幕侧兜底。

### 9.7.4 1/4 查表：中心样本 + 抖动样本 + rank 0 过滤 + L1 压缩

**做什么**

- 每个 1/4 像素按 §0.0 的网格口径落 3 个样本点，各自 `world_from_depth` 反投影 + 平面法线（与播种同源，或按 `normal_source` A/B 取 G-buffer 着色法线）；查缓存得 RGBL1。
- 样本规则照 Unity：中心样本权重 0.01 且**不查 rank**（代表"这里有没有缓存"），抖动样本只接受 `rank == 0`；位置在体素尺寸内抖动、方向在 `cos = 0.9` 的锥内抖动（序列复用 9.6 的 `QrngKronecker2D`，同帧同像素确定）。
- 加权合成后做 L1 压缩写 `scgi_l1`（`l1 × √3/(4·l0) + 0.5`，`l0 → 0` 时按 0.5 兜底防除零），L0 写 `scgi_l0.rgb`；`a` = 命中 rank 0 的样本权重占比（Volume 外或无命中 = 0）。
- Volume 外不查缓存、直接 `a = 0`（回退 IBL 基线，不在这里求 9.3 的 probe）。

**落点**：`shaders/scene/gi/screen_gather.wgsl`（新）、`src/render/passes/gi/screen_gather.rs`（新）、`shaders/common/gi_compression.wgsl`（新，编解码一对）、`src/render/passes/gi/mod.rs`（目标与管线）。

**坑 / 钉子**

- 1/4 深度取样用 `textureLoad`（D32 不可线性过滤），**绝不用采样器**——平均深度反投影是错的几何（9.5 同款钉子）。
- 命中率是判定线④的读数：命中/拒绝/越界三个计数要进计数表（`GI_COUNTER_WORDS = 16` 已用满 0..15，扩到 20 或复用读零位，见 §5）。
- `a` 是覆盖权重不是置信度：别把"查到 rank 1 被拒"混进权重，否则漏光会从权重里漏回来。

### 9.7.5 去噪与全分辨率双边上采样

**做什么**

- 去噪：0～N 遍（默认 1 遍，可关）3×3，第 k 遍步长 `{1,2,2,3}[k%4]`；权重 = 高斯 × 法线相似度（点积的 8 次方）× 平面距离衰减，只在法线/距离相似时混合，跨边缘不混。
- 上采样：全分辨率像素取 4 个 1/4 邻居，权重 = 双线性 × 法线点积 × 平面距离指数衰减 + 0.01 倍双线性兜底；用**全分辨率着色法线**对 L0+L1 求值 `E(n)` 写 `scgi_irradiance`；`a` = 4 邻居覆盖率的最大值（Volume 边界用）。
- 开关 `upsample_bilinear`：对照"直接双线性"与"双边"两版（判定线⑤读图）。

**落点**：`shaders/scene/gi/denoise.wgsl`（新）、`shaders/scene/gi/upsample.wgsl`（新）、`src/render/passes/gi/screen_gather.rs`、`src/render/frame/render_features.rs`（新目标句柄）。

**坑 / 钉子**

- L1 解码与 L0 必须同源：解码后 `E = l0 + l1·n`，基函数常数已折在 `PatchIrradiance` 里，**不能再乘 1/π 或余弦卷积**（那是消费端 `albedo·Fd_Lambert()` 的事）。
- 上采样必须读去噪之后的 1/4 目标：顺序错就是"拿没去噪的值插值"，读图见块状（判定线⑤）。
- 全分辨率目标随 swapchain 重建：布局只在 init 建一次、resize 只重建附件与 pool（2026-11-24 `gbuffer_targets.rs` 的现场 VUID 就是这个教训）。

### 9.7.6 注入：合成 pass 第 6 集与 `scgi` 开关

**做什么**

- 帧 UBO 336 → 352B：新增 `scgi: vec4u`（x = 开关，y = 预留质量档，zw 保留）；同步 `FRAME_UBO_SIZE`、布局镜像断言、`pack_frame_uniforms`、`FrameUniformsData::default` 与 WGSL `FrameUniforms`。
- `ibl_environmental()` 内加一条替换：`scgi.x == 1 && irr.a > 0` 时 `terms.diffuse = pixel.diffuseColor · Fd_Lambert() · irr.rgb`（是否仍乘 `ibl_intensity` 见 §5），高光不动。
- 合成 pass 第 6 集：b0 = `scgi_irradiance`（`textureLoad`，1/4 已上采样过，不再过滤）；`scgi` 关时绑一张常驻 1×1 零纹理——**pipeline layout 恒定**，避免"关 = 基线"还要重建管线才能比对。
- 默认值：`scgi = false`（与 `ibl_enabled` 默认关同纪律，BRP/面板开）。

**落点**：`src/render/resident/tables/frame_uniform_table.rs`、`shaders/common/frame.wgsl`（或等价片段）、`shaders/scene/gbuffer/gbuffer_composite.wgsl`、`src/render/passes/lighting/composite_pipeline.rs`（第 6 集）、`src/render/frame/record.rs:290-305`（sets）、`src/render/passes/gi/mod.rs`（设置与目标所有者）。

**坑 / 钉子**

- 背景分支（`depth == 0`）与调试模式（`frame.mode != 0`）在替换点之前就 return 了，无需额外判断；但模式 9/10/11（IBL 漫反射/高光/桶）**不得**被 SCGI 污染——插入点必须落在 `MODE_SHADING` 路径内。
- `a = 0` 区域必须逐像素回 IBL：用 `mix` 时两边都要算；别把 IBL 一侧写成 0 再 mix（整屏变暗）。
- 关态逐位一致要在**同一进程同一帧槽**取（跨进程 PPM 因驱动排序假失败，9.5 的教训）。

### 9.7.7 调试视图、读数与判定线

**做什么**

- 视图 14～17（`gi_set.view` 上限扩到 17）：14 屏幕辐照度（乘 `EXPOSURE`）、15 覆盖率/查表命中、16 rank 拒绝率、17 方差与去噪前后对照。
- BRP 读数（`gi_stats` 扩字段）：`scgi_ms`（④.5 内新增区间或并入 `gi_ms`，见 §5）、查表命中/拒绝/越界计数、平均覆盖率、1/4 与全分辨率目标显存。
- 参数面：`scgi` / `spatial_filter` / `temporal_filter` / `temporal_smoothing` / `denoise_passes` / `lookup_samples` / `upsample_bilinear` 走 `gi_set` 与面板同源。
- 判定线与计量：`tools/gi_gather_test.py`（新，与 `gi_estimation_test.py` 同形：BRP 开关组合 → `gi_stats` 读数 → `gi_dump` 取图）+ 全套回归（`gi_test` / `gi_estimation_test` / `hiz_test` / `ray_tracing_test` / `scene_*`）。

**落点**：`shaders/scene/gi/screen_view.wgsl`（新）、`src/render/passes/gi/view.rs`、`src/remote/features/gi.rs`、`src/overlay/tool_window/gi.rs`、`src/render/stats/perf.rs`、`tools/gi_gather_test.py`（新）、`9.7.7-判定线与计量.md`（施工记录）。

**坑 / 钉子**

- 调试图不走后链（不 Bloom、不 tonemap 两遍）：沿用 9.5 的 LDR 直出通道，与 patch 视图同一条。
- 视图 14 的色阶要先实测再标定（9.6.6 的教训：视图 10/11 原先 `E ≈ 4` 就饱和、整屏发白，之后才标定）。
- 判定线读数必须成组：分辨率档 × 实例档 × 开关态，不拿单张截图代替。

## 3. 前置闸门（开工首日按序关闭）

1. **空 dispatch 打通**：1/4 全屏 compute 写一张 RGBA16F、零 VUID、`gi_ms` 有增量；合成 pass 第 6 集绑 1×1 零纹理时画面与 9.6 收官**逐位一致**。
2. **开关闭环**：`scgi` 关（生产者开/关两种）都逐位回 9.6 收官；`scgi` 开但 `gi.enabled` 关 = 全屏回 IBL（不是全黑）。
3. **1/4 网格口径**：gather 与播种的 1/4→全分辨率映射同一份（含 `sub = (frame*7)%16`），单测钉住；否则命中率读数不可信。
4. **rank 过滤取证**：把 rank 视图固定成"只显示 rank 1"，屏幕查表结果不出现该 patch 的值（判定线④的可执行形式）。
5. **帧 UBO 扩字段契约**：336 → 352B 后布局镜像断言、WGSL 镜像、`pack_frame_uniforms` 三处一致，`layout_contract_tests` 全过、零 VUID。
6. **目标随 resize**：改窗口尺寸 3 次不 VUID、不泄漏（"布局只建一次"的不变量照 9.5 的现场教训写测试）。

## 4. 显存与性能口径

- 新增显存（1080p 起步档）：
  - `filtered` 辐照度双缓冲：65536 × 64B × 2 ≈ 8 MiB；
  - 1/4 目标双缓冲：`scgi_l0` 480×270×8B×2 ≈ 2.0 MiB + `scgi_l1` 480×270×4B×2 ≈ 1.0 MiB；
  - 全分辨率 `scgi_irradiance`：1920×1080×8B ≈ 15.8 MiB；
  - 合计 ≈ 27 MiB，与 9.5 的 ≈30.5 MiB、9.6 的 <4 MiB 同场记录；4K 时全分辨率那张涨到 ≈63 MiB（进 9.8 压测复核）。
- 每帧成本：① ② 各 65536 线程（活跃内才有工作）；③ ④ 各 `(w/4)/8 × (h/4)/8` 组；⑤ `(w/8) × (h/8)` 组。**先写后测**：1080p 首版目标 ③④⑤ 合计 ≤ 1.5 ms、①② ≤ 0.5 ms（2060 Release 实测为准，不拿"流畅"替代读数）。
- 读数成组：分辨率（1080p/1440p）× 实例档（1 万/5 万）× 开关态；每档记 `scgi_ms`、命中/拒绝率、平均覆盖率、`gi_ms`、显存。判定线⑥要的正是"实例数翻倍而分辨率不变时曲线不动"。
- 调参优先级：先拿到命中率 > 覆盖率 > 噪声三个读数，再谈毫秒；命中率低时先查 1/4 网格口径（闸门 3），不要靠加样本掩盖。

## 5. 待决项（施工时钉明，不预设结论）

| 待决 | 选项 | 裁决点 |
|---|---|---|
| `ibl_intensity` 是否乘到 SCGI | 乘（与 IBL 同域，A/B 只换辐照度源）/ 不乘（缓存已是绝对辐照度） | 9.7.6 首次 A/B：`sky_uniform` 常量天空下 SCGI 与 IBL 基线应接近；若差一个常量倍率即知该不该乘 |
| fp16 动态范围 | RGBA16F 直存物理单位 / RGB9E5（无 alpha）/ 乘固定曝光后存、注入时除回 | 9.7.4 实测天空与自发光峰值辐亮度（`EXPOSURE = 1.6e-4` 暗示峰值在 10³ 量级，fp16 够；超 6.5e4 才换） |
| 时域后滤波是否保留 | 保留（Unity 同款）/ 只留 estimation 的方差引导 | 判定线⑤的闪烁读图；若 9.7.3 收益不可见就砍掉并记录 |
| 去噪遍数默认 | 0 / 1 / 2 | 1/4 目标的噪声与 `scgi_ms` 权衡，读图定 |
| 查表样本数默认 | 3（中心 + 对角 2）/ 5 / 9 | 命中率与噪声；Unity 默认 `LookupSampleCount` 是 4 |
| 覆盖率 `a` 的口径 | 命中样本权重占比 / 命中即 1（硬掩码）/ 按距离衰减 | 判定线⑤的 Volume 边界读图：软过渡更顺但"半接管"，硬掩码更利但切边 |
| 计数扩容 | `GI_COUNTER_WORDS` 16 → 20 / 复用未用位 / 另立 SCGI 计数缓冲 | 9.7.4 首次读数：加三个计数就要改 UBO 大小与 BRP 解析 |
| 计时点位 | 新增 `Scgi` 点位（12 → 13，按 9.5 先例）/ 并入 `SurfaceCache`（现状） | 9.7.7 首次读数：`gi_ms` 若无法把 gather 与 estimation 分开就加点位 |
| 上采样是否重算 Volume 归属 | 读 1/4 alpha（Volume 边界软）/ 全分辨率重算 `resolve_cascade` | 边界处法线/距离权重的表现；重算要绑 GI UBO 与 cell 表进片元，成本高 |

## 6. 与前后段的接口

- **9.2（IBL）**：SCGI 替换的正是 `ibl_environmental()` 的 `diffuse` 项；高光与 `ibl_intensity` 口径不动。`ibl_enabled = false` 时 SCGI 也不许生效（没有基线可比，且缓存种子会是 0）——开关层叠顺序 `sky → ibl → scgi` 照[注入端](../../../笔记/光照与材质/SCGI间接光注入端.md)§1。
- **9.3（GlobalProbe）**：Volume 外**不改** `frame.ibl_mode` 的选择逻辑，就是 IBL 基线本身（`ibl_mode = 1` 的 A/B 仍有效）。
- **9.5（patch 池）**：只读 patch 几何/统计/辐照度与 cell 表，不写任何 9.5 结构；`filtered` 是新增表，不改 ring/cell/rank 语义。1/4 网格口径与播种同源（§0.0）。
- **9.6（estimation）**：读本帧写的那份辐照度（奇偶由 GI UBO `frame.x & 1` 给出）；`update_count`/`variance` 作"是否参与滤波"的门限。抽出的只读查表反过来被 estimation 复用，**行为逐位不变**是硬线。
- **9.8（动态验收与压测）**：本段的开关面、读数、漏光案例（判定线⑦）、成本曲线直接进压测与局限对照；默认值（`scgi`/滤波/去噪）的最终拍板在 9.8，不在本段。

## 判定线草案（段级汇总）

1. **SCGI 关 = IBL 基线**（逐位）：`scgi = 0` × `ibl_mode = 0/1` 两种编码 × `ibl_enabled = 0/1` 两种基线，四组都与 9.6 收官帧逐字节一致。
2. **常量天空自检**：`sky_uniform` 打开（`E = πL` 的精确锚）且 Volume 覆盖满屏时，SCGI 注入后的显示值与 IBL 基线在同一区间，差 ≤ 阈值（首测标定，先记读数不预设通过线）——这条是"注入公式没写错"的量化证据。
3. **覆盖范围分工**：Volume 外不接管（`a = 0` 区域逐位回 IBL）、透明层不接（A/B 读图 + 代码路径举证）、背景与调试模式不接。
4. **rank 1 不被消费**：强制"只显示 rank 1"视图下，屏幕查表读数不出现该 patch 的值；拒绝计数 > 0 时命中率读数仍自洽。
5. **上采样质量**：无法线边界的块状、无高对比边缘的振铃；Volume 边界一圈过渡不产生明显亮/暗环（读图留档）。
6. **成本随分辨率不随实例数**：同分辨率下实例数 1 万 → 5 万，`scgi_ms` 曲线不抬（成组读数）；分辨率 1080p → 1440p 时按像素数近似线性。
7. **漏光案例留档**：薄墙、双面几何、桌面/箱底三类场景读图对照，与 9.6 的 CSM 覆盖差漏光分开记（八段 §2 9.7 明列）。
8. **零 VUID、`cargo fmt --check` 绿、clippy 零告警、库测试全过**；关态零新增命令与屏障。
9. **回退链完整**：`scgi = 0` → IBL；`ibl_enabled = 0` → 步骤 8 的 `albedo × ambient_color`；`sky_enabled = 0` → 清屏色；三层各回上一层且逐位一致。
