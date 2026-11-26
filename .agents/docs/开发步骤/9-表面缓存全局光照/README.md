# 步骤 9：天空盒、IBL 与表面缓存全局光照（M8）

对应[执行路线图](../学习目标实现步骤.md)。本 README 是步骤索引与材料目录；各段任务面板在段目录 README（开工时立），段施工计划随段立案入段目录。

**目的**：步骤 8 已把管线补成完整延迟管线，但背景是清屏色，间接光仍是一个常量环境项，没有天空、没有 IBL、没有 GI。本步先补环境基线——程序化天空 → 标准 split-sum IBL（漫反射 SH-L2、高光预滤波 cubemap + DFG LUT）——再按 Unity SCGI 范式做世界空间表面缓存：把稀缺射线花在贴表面的缓存 patch 上、多跳靠缓存自查询、屏幕侧 1/4 分辨率 gather 后替换 Volume 内的 IBL 漫反射。

**一句话说明**：先用程序化天空和 IBL 把环境光从常量色升级成有方向性的基线，再用 GlobalProbe 16 桶建立与 patch 同构的兜底，最后用"空间格子 + 方向桶 + 定长 patch 池"缓存世界空间表面辐照度、少量射线维护、屏幕侧查缓存→滤波→上采样→注入；本步只主张间接漫反射，不主张 SSR/局部反射探针、AO 与烘焙。

**当前状态：🚧 施工中——9.1 程序化天空盒、9.2 天空盒 IBL 与 9.3 GlobalProbe 方向桶已收官（2026-11-23；9.2 含 9.2.8 现场复检补修，9.3 含 L2/16 桶 A/B 与七条判定线），9.4 求交器与射线基建已收官（2026-11-24，硬件 ray query + BLAS/TLAS + 命中重建 + BRP 四口，判定线 15 项与五脚本回归全过），9.5 patch 池与分配回收已收官（2026-11-24，patch 池 + 回收三件 + 调试视图两形态，判定线 25/25 与五脚本回归全过），9.6 irradiance 更新已收官（2026-11-24，辐照度估计 + 多跳 + 预算 + 视图 9～13，判定线 12/12 与全套回归全过，[段 README](9.6-irradiance更新/README.md)），9.7 屏幕 gather 与最终注入主体收官（2026-11-24；1/4 查表 + 空间/时域滤波 + 全分辨率双边上采样 + 注入，判定线 7/9 闭环、2 项部分，视图 16/17 与 `scgi_ms` 按待决顺延，[段 README](9.7-屏幕gather与注入/README.md)），9.8 动态验收、压测与局限对照已立案（2026-11-26 用户要求先做 9.8；普通功能段：动态验收、压测与局限对照，不是本步收尾，后续仍有新任务追加，[施工计划](9.8-动态验收与压测/施工计划：动态验收、压测与局限对照.md)）；2026-11-26 用户增补 9.9 ToneMapping 颜色校验面板与 9.10 SCGI 与 Unity7 URP 对照审计（均未开工，编号追加在 9.8 之后，施工顺延到 9.8 之后）。** 八段施工计划见[施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)（2026-11-20 重规划），9.4 段细化计划见[施工计划：求交器与射线基建](9.4-求交器与射线基建/施工计划：求交器与射线基建.md)（2026-11-24 立案），9.5 段细化计划见[施工计划：patch 池与分配回收](9.5-patch池与分配回收/施工计划：patch池与分配回收.md)（2026-11-24 立案），9.6 段细化计划见[施工计划：irradiance 更新与多跳](9.6-irradiance更新/施工计划：irradiance更新与多跳.md)（2026-11-24 立案）；9.7 段细化计划见[施工计划：屏幕 gather 与最终注入](9.7-屏幕gather与注入/施工计划：屏幕gather与最终注入.md)（2026-11-24 用户增补立案，含"per-pixel → 1/4 查表 + 全分辨率上采样"与"撤 Chebyshev 软可见性"两处口径订正），9.8 段细化计划见[施工计划：动态验收、压测与局限对照](9.8-动态验收与压测/施工计划：动态验收、压测与局限对照.md)（2026-11-26 用户要求先做 9.8 立案），9.9 段细化计划见[施工计划：ToneMapping 颜色校验面板](施工计划：ToneMapping颜色校验面板（9.9 追加）.md)（2026-11-26 用户增补立案），9.10 段细化计划见[施工计划：SCGI 与 Unity7 对照审计](施工计划：SCGI与Unity7对照审计（9.10 追加）.md)（2026-11-26 用户增补立案）。 SCGI 选型仍是 **Unity SCGI 骨架 + Kajiya 三件借用**（空间格子 + 16 方向桶 + 定长 65536 patch 池 + 屏幕播种 + rank/heartbeat/defrag；借法线偏置、rank 不变式、投票分配器备件）；天空/IBL 选型见施工计划 §1.5。理由与推翻判据见施工计划 §1。

## 参照实现（Unity 7 URP 源码）

遇到实现问题（屏幕 resolve、时域/空间去噪、上采样注入、分配回收、兜底材质等）时，**可以直接看 Unity 7 URP 的实现，允许源码级照抄**；移植到本仓库时按 WGSL / ash 口径重写，并在注释里注明来源文件名与函数名，方便复核。

- 参考工程：`F:\okzkx\TestSCGI`（`com.unity.render-pipelines.universal` 17.7.0）
- URP 源码：`F:\okzkx\TestSCGI\Library\PackageCache\com.unity.render-pipelines.universal@f74a61fa3847`
- SCGI 参考实现目录：`Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\`
  - `ScreenResolveLookup.compute`——1/4 屏幕查表（gather）
  - `ScreenResolveDenoising.compute`——去噪（空间/时域）
  - `ScreenResolveUpsampling.compute`——全分辨率上采样与注入
  - `FlatNormalResolution.compute`——法线重建；`PatchAllocation.compute`——patch 分配与回收
  - `Debug.compute`——调试视图；`FallbackMaterial.shader`——无缓存区域的兜底
  - `SurfaceCacheGIRendererFeature.cs`、`SurfaceCacheWorldAdapter.cs`——pass 顺序、资源生命周期与场景数据接驳

## 施工顺序

| 段 | 文件夹（开工时立） | 范围 | 状态 |
|---|---|---|---|
| 9.1 | [9.1-程序化天空盒](9.1-程序化天空盒/README.md) | 物理天空：三介质单次散射（瑞利/米氏/臭氧）+ Chapman 透射率 + 地面反照率 + 太阳圆盘；合成 pass 背景替换清屏色；参数进帧 UBO（304B）；`sky_enabled`；BRP `sky_set` + 天空面板 | ✅ 收官（2026-11-23，[施工记录](9.1-程序化天空盒/README.md#施工记录)） |
| 9.2 | [9.2-天空盒IBL](9.2-天空盒IBL/README.md) | 物理天空捕获成环境 cubemap（256²×6，脏标记 + 节流，排太阳圆盘）；SH-L2 漫反射；GGX 预滤波 + DFG LUT；不透明/透明环境项替换为 IBL；`ibl_enabled`；`sky_uniform` 常量天空自检开关 | ✅ 收官（2026-11-23，含 9.2.8 现场复检补修；[段 README](9.2-天空盒IBL/README.md)、[判定线](9.2-天空盒IBL/9.2.7-判定线与计量.md)） |
| 9.3 | [9.3-GlobalProbe方向桶](9.3-GlobalProbe方向桶/README.md) | 16 桶 × RGBL1（4×4 八面体，32 样本/桶）；法线选桶求值；`ibl_mode` L2/16 桶 A/B；为 9.7 体积外兜底建格式 | ✅ 收官（2026-11-23；A/B 保留 SH-L2 默认；[段 README](9.3-GlobalProbe方向桶/README.md)、[判定线](9.3-GlobalProbe方向桶/9.3.5-判定线与计量.md)） |
| 9.4 | [9.4-求交器与射线基建](9.4-求交器与射线基建/README.md) | 硬件 ray query 主后端 + 可替换接口；BLAS 挂驻留资产（每组最粗已驻留变体一份）、TLAS 每帧从在场环构建；命中点属性从既有网格池重建；调试图与 BRP 取证 | ✅ 收官（2026-11-24；[段 README](9.4-求交器与射线基建/README.md)、[判定线](9.4-求交器与射线基建/9.4.7-判定线与计量：ray_tracing_test十五项与五脚本回归.md)） |
| 9.5 | [9.5-patch池与分配回收](9.5-patch池与分配回收/README.md) | Volume（球判据 + 立方存储，Res 32 / 4 级联 / Size 128）+ 16 方向桶 + 二层 cell 表（`CellPatchIndices`/`CellAllocationMarks`）+ 定长 65536 patch 池；1/4 屏幕播种（4×4 子像素 16 帧轮转）、cell 命中保历史、滚动/心跳/整理三件回收、rank 语义；patch 视图两形态（表面覆盖图 + 几何标记）+ BRP 四口 | ✅ 收官（2026-11-24；[段 README](9.5-patch池与分配回收/README.md)、[判定线](9.5-patch池与分配回收/9.5.7-判定线与计量.md)） |
| 9.6 | 9.6-irradiance更新 | 每活跃 patch 均匀半球采样（Kronecker 2D + 预热倍率，样本档 1/2/4/8）；直接光三路（方向光低档 PCF / 光源侧点聚光清单 / 自发光三角形 + MIS）与缓存自查询多跳（读上一帧，bounce 分配 rank 1）；L1 投影 + 余弦卷积 + 累计平均→方差引导时域混合；射线预算硬约束与降采样；调试视图（样本档/射线数/方差/更新次数/rank）与参数走 egui 面板 + BRP 同源 | ✅ 收官（2026-11-24；[段 README](9.6-irradiance更新/README.md)、[判定线](9.6-irradiance更新/9.6.7-判定线与计量.md)、[流程走读](9.6-irradiance更新/irradiance更新与多跳流程.md)） |
| 9.7 | [9.7-屏幕gather与注入](9.7-屏幕gather与注入/README.md) | patch 级空间滤波（切平面抖动 taps，权重 `1 − \|jitter\|/√2`）+ 时域后滤波（`lerp(0.9, 0.95)`）；1/4 查表（中心样本 + 抖动样本 + rank 0 过滤，位置/方向抖动）+ L1 压缩（L0 RGBA16F / L1 RGBA8）+ 3×3 去噪 → 全分辨率双边上采样并按着色法线求值；Volume 内替换 IBL 漫反射、体积外回 IBL 基线（`ibl_mode` 决定 SH-L2 还是 16 桶）；`scgi` 独立开关 + 屏幕侧调试图 | 🚧 施工中（2026-11-24 开工，[段 README](9.7-屏幕gather与注入/README.md)、[施工计划](9.7-屏幕gather与注入/施工计划：屏幕gather与最终注入.md)） |
| 9.8 | [9.8-动态验收与压测](9.8-动态验收与压测/施工计划：动态验收、压测与局限对照.md) | 动态几何/动态光照（含 TOD 天空）收敛行为；Volume 边界与兜底；成组压测；已知局限逐项自检 | ⬜ 已立案（2026-11-26 用户要求先做 9.8） |
| 9.9 | 9.9-ToneMapping颜色校验面板（开工时立） | 后处理链三点（原始 HDR / 校色输入 / 校色输出）的 GPU 直方图与 egui「颜色」页：亮度 log2 轴 + RGB 分布 + CbCr 色度 + 钳位/百分位/均值 + 钉住基线 A/B；曝光控件补面与增益滑杆修正；BRP `color_scope_get` + CSV 复核脚本；读数只读，另加**校色强度**（曲线末端混合系数，0–1，默认 1.0 输出不变）手动滑杆 + BRP `bloom_set` 可调；不改曲线与曝光算法 | ⬜ 已立案（2026-11-26 用户增补，[施工计划](施工计划：ToneMapping颜色校验面板（9.9 追加）.md)） |
| 9.10 | 9.10-SCGI与Unity7对照审计（开工时立） | 与 Unity URP 17.7 的 40 个 SCGI 源文件逐文件/逐 kernel 对照：缺漏/冗余/实现错误三张清单 + ≥20 项参数对照 + 运行时证据；P0 小修闭环、结构性项另立；只审 SCGI 栈（9.3–9.7 + 场景接驳/BRP/调试视图） | ⬜ 已立案（2026-11-26 用户增补，[施工计划](施工计划：SCGI与Unity7对照审计（9.10 追加）.md)） |

一次只施工一段：9.1 天空 → 9.2 IBL → 9.3 GlobalProbe → 9.4 求交器 → 9.5 建池 → 9.6 出辐照度 → 9.7 接屏幕 → 9.8 动态验收与压测（普通段，非收尾）。**顺序不可倒**：IBL 是 SCGI 的对照基线，求交器是缓存的前置；9.4 与 9.3 可并行，但 9.7 必须等 9.2/9.3/9.5/9.6 全部就位。9.8 于 2026-11-26 按用户要求先立案（普通功能段，施工完并不代表步骤 9 结束）；9.9/9.10 是同日增补段，编号追加在 9.8 之后、施工顺延到 9.8 之后——9.9 给 9.10 的修复验收提供读数工具，9.10 的 P0 修复原建议在 9.8 压测前闭环，本次按用户指定先做 9.8，凡依赖 9.10 的读数在 9.8 报告里标"待 9.10 后复测"。逐段独立验收以[路线图步骤 9 范围拆解表](../学习目标实现步骤.md)的验收重点列为准；头号待决项见[施工计划 §3](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)。

## 参数基线（6GB 显存账）

| 阶段 | Res | 级联 | Size | 最细体素 | 覆盖 | cell 表（单张 / 双表） | patch 池 |
|---|---|---|---|---|---|---|---|
| 9.5 起步 | 32 | 4 | 128 m | 0.5 m | 128 m | 8 MiB / 16 MiB | 65536 条 ≈ 12 MiB |
| 大世界档 | 32 | 6 | 512 m | 0.5 m | 512 m | 12 MiB / 24 MiB | 同上（可提 2^18 ≈ 48 MiB） |
| 高品质候选（二期） | 64 | 4 | 128 m | 0.25 m | 128 m | 64 MiB / 128 MiB；按级联降桶 16/8/4/4 → 32 MiB / 64 MiB | 同上 |

覆盖靠加级联（线性内存）而不是加分辨率；一期就用满 16 桶以对齐 Unity 源码。完整推导与显存口径见[施工计划 §1.3](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)。

**IBL 侧另计**（9.1～9.3）：环境 cubemap 256²×6 RGBA16F ≈ 3 MiB（mip 链约 4 MiB），预滤波 cubemap 同量级，DFG LUT 128² ≈ 64～128 KiB（RG16F/RGBA16F），SH-L2 144B，GlobalProbe 1KB。与贴图预算相比可忽略，但仍按施工计划 §7 成组记录。

## 开工前置

- 步骤 8 已收官（8.1～8.6）；步骤 5/6 的步骤级验收归档欠账不阻塞本步。
- F25/F26 挂账不阻塞本步（[待修复清单](../待修复清单/待修复清单.md)）。
- 设备能力：本机 RTX 2060 已核实支持 `VK_KHR_acceleration_structure` / `VK_KHR_ray_query` / `VK_KHR_ray_tracing_pipeline` / `VK_KHR_ray_tracing_position_fetch`；**9.1～9.3 不依赖 光线求交**，9.4 起才要求。不支持 光线求交时 GI 走关闭路径（9.4 feature 门）。
- 验收场：Sponza（室内基座/漏光）、zero_day（室外天空与规模）、DamagedHelmet（金属/粗糙度高光）、大世界档（Size 512）；Unity 251 场景与 Atrio 只作质量参照。
- 目录组织：天空/IBL 落 `render/passes/sky/`、`render/passes/ibl/`；GI 代码落 `render/ray_tracing/`、`render/resident/tables/surface_cache.rs`、`render/passes/gi/`；shader 落 `shaders/common/{sky,ibl,octahedral,gi_cell,raytracing}.wgsl`、`shaders/scene/{ibl,gi,ray_tracing}/`（[施工计划 §5](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)）。

## 材料清单

- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——选型结论（含天空/IBL §1.5）、八段拆解、参数基线、待决项与边界（2026-11-20 立案/重规划）。
- [9.1 段任务面板](9.1-程序化天空盒/README.md)与[施工计划：程序化天空盒](9.1-程序化天空盒/施工计划：程序化天空盒.md)——天空模型、UBO 布局与判定线；[功能流程导读](9.1-程序化天空盒/程序化天空：从参数到背景像素.md)（2026-11-23 收官）。
- [9.2 段任务面板](9.2-天空盒IBL/README.md)与[施工计划：天空盒IBL](9.2-天空盒IBL/施工计划：天空盒IBL.md)——捕获、SH-L2、预滤波、DFG LUT、注入与描述符布局。
- [9.3 段任务面板](9.3-GlobalProbe方向桶/README.md)与[施工计划：GlobalProbe方向桶](9.3-GlobalProbe方向桶/施工计划：GlobalProbe方向桶.md)——八面体映射、16 桶 L1、A/B 口径与兜底格式；功能流程见[GlobalProbe 方向桶流程](9.3-GlobalProbe方向桶/GlobalProbe方向桶流程.md)（2026-11-23 收官）。
- [9.4 段施工计划](9.4-求交器与射线基建/施工计划：求交器与射线基建.md)——设备能力门、BLAS/TLAS 生命周期、命中几何取数、调试图与判定线（2026-11-24 立案并收官，[段 README](9.4-求交器与射线基建/README.md)）。
- [9.5 段施工计划](9.5-patch池与分配回收/施工计划：patch池与分配回收.md)——现状核实清单（含"深度可采样"旧结论订正）、Volume/池容量/播种/回收四组裁决、数据布局与显存账、patch 调试视图两形态（表面覆盖 + 几何标记）、9.5.1～9.5.7 任务、判定线六条草案与前置闸门（2026-11-24 立案并收官；[段 README](9.5-patch池与分配回收/README.md)、[功能流程导读](9.5-patch池与分配回收/patch池与分配回收流程.md)）。
- [patch 池与格子系统讲解](9.5-patch池与分配回收/patch池与格子系统讲解：法线口径、播种网格与级联排列.md)——三件口径的推导：为什么不用 G-buffer 法线、1/4 播种网格是什么、级联排列的算术（含数值走查与"存储槽与相机无关"的推论），以及调试视图该回答的四个问题（2026-11-24）。
- [Unity SCGI 调研](../../笔记/光照与材质/Unity%20SCGI%20调研.md)——发布状态、运行模型、参数面、已知局限、测试资源与调研来源。
- [Unity SCGI 实现](../../笔记/光照与材质/Unity%20SCGI%20实现.md)——源码级数据流：Volume/cell/patch 池、分配与回收、采样、四级滤波、GlobalProbe、Kajiya 对照与收益代价。
- [全局光照谱系调研](../../笔记/光照与材质/全局光照谱系调研.md)——四族谱系、选型阶梯与"查询端 → 求交器 → 动态缓存"的顺序约束。
- [Filament BRDF参考](../../笔记/光照与材质/Filament%20BRDF参考.md)——标准 BRDF、split-sum/DFG、`energyCompensation` 与预曝光口径。
- [SH 与光照探针](../../笔记/光照与材质/SH与光照探针.md)——探针的 5D 光照场、L1/L2、GlobalProbe 16 桶与 Cell 16 槽的区别。
- 本机源码位置（2026-11-20 核实；执行方式=可模仿/照抄，许可边界见[施工计划 §8.1](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)）：
  - Unity core：`F:\Program Files\Unity\7000.0.0a9\Editor\Data\Resources\PackageManager\BuiltInPackages\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\`（`SurfaceCache.cs`、`SurfaceCacheWorld.cs` 与各 compute/hlsl）；
  - Unity 环境 cubemap：`...\com.unity.render-pipelines.core\Runtime\PathTracing\Environment\CubemapRender.cs`；
  - Unity URP：`...\com.unity.render-pipelines.universal\Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\`（`SurfaceCacheGIRendererFeature.cs`、`SurfaceCacheWorldAdapter.cs`、`ScreenResolve*`）；
  - Unity 实跑工程：`F:\okzkx\TestSCGI`、`F:\okzkx\TestUnity67`、`F:\okzkx\Unity-Atrio-Demo`；
  - Filament：`F:\okzkx\filament`（`shaders/src/surface_lighting.fs` 等）；
  - Kajiya：本机未检出；执行前 `git clone --depth 1 https://github.com/EmbarkStudios/kajiya F:\okzkx\kajiya`（`docs/gi-overview.md`、`crates/lib/kajiya/src/renderers/ircache.rs`、`assets/shaders/ircache/`）。
- [Unity 6.7 手册：Surface Cache Global Illumination in URP](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi.html)——启用步骤、质量调优、自定义 Shader、排障与 Volume 参考。
- [官方 251 场景：251_SurfaceCache_URP_GI.unity](https://github.com/Unity-Technologies/Graphics/blob/6000.7/staging/Tests/SRPTests/Projects/UniversalGraphicsTest_Lighting/Assets/Scenes/251_SurfaceCache_URP_GI.unity)——受控 A/B 参照（Size 20 / Res 128 / 1 级联 / 32 样本）。
- [Atrio 官方 Demo](https://github.com/Unity-Technologies/Unity-Atrio-Demo)——Unity 侧画质参照（无 lightmap，SCGI 主打）；[社区 Cornell Box](https://github.com/Arlorean/UnitySCGI-CornellBox)——最小复现场。
- [8.1 G-buffer 段](../8-完整延迟渲染管线/8.1-G-buffer与不透明几何/)——G-buffer 三原料与 motion vectors 附件位（9.5 前置件）。
- [8.3 CSM 段](../8-完整延迟渲染管线/8.3-CSM与PCSS级联阴影/)——9.6 直接光遮挡复用的阴影链。
