# 步骤 9：天空盒、IBL 与表面缓存全局光照（M8）

对应[执行路线图](../学习目标实现步骤.md)。本 README 是步骤索引与材料目录；各段任务面板在段目录 README（开工时立），段施工计划随段立案入段目录。

**目的**：步骤 8 已把管线补成完整延迟管线，但背景是清屏色，间接光仍是一个常量环境项，没有天空、没有 IBL、没有 GI。本步先补环境基线——程序化天空 → 标准 split-sum IBL（漫反射 SH-L2、高光预滤波 cubemap + DFG LUT）——再按 Unity SCGI 范式做世界空间表面缓存：把稀缺射线花在贴表面的缓存 patch 上、多跳靠缓存自查询、屏幕侧 1/4 分辨率 gather 后替换 Volume 内的 IBL 漫反射。

**一句话说明**：先用程序化天空和 IBL 把环境光从常量色升级成有方向性的基线，再用 GlobalProbe 16 桶建立与 patch 同构的兜底，最后用"空间格子 + 方向桶 + 定长 patch 池"缓存世界空间表面辐照度、少量射线维护、屏幕侧查缓存→滤波→上采样→注入；本步只主张间接漫反射，不主张 SSR/局部反射探针、AO 与烘焙。

**当前状态：🚧 施工中——9.1 程序化天空盒与 9.2 天空盒 IBL 已收官（2026-11-23；9.2 含 9.2.8 现场复检补修，[段 README](9.2-天空盒IBL/README.md)），9.3 GlobalProbe 方向桶施工计划已按 9.2 落地复核、未开工，9.4～9.8 未开工。** 八段施工计划见[施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)（2026-11-20 重规划）。 SCGI 选型仍是 **Unity SCGI 骨架 + Kajiya 三件借用**（空间格子 + 16 方向桶 + 定长 65536 patch 池 + 屏幕播种 + rank/heartbeat/defrag；借法线偏置、rank 不变式、投票分配器备件）；天空/IBL 选型见施工计划 §1.5。理由与推翻判据见施工计划 §1。

## 施工顺序

| 段 | 文件夹（开工时立） | 范围 | 状态 |
|---|---|---|---|
| 9.1 | [9.1-程序化天空盒](9.1-程序化天空盒/README.md) | 物理天空：三介质单次散射（瑞利/米氏/臭氧）+ Chapman 透射率 + 地面反照率 + 太阳圆盘；合成 pass 背景替换清屏色；参数进帧 UBO（304B）；`sky_enabled`；BRP `sky_set` + 天空面板 | ✅ 收官（2026-11-23，[施工记录](9.1-程序化天空盒/README.md#施工记录)） |
| 9.2 | [9.2-天空盒IBL](9.2-天空盒IBL/README.md) | 物理天空捕获成环境 cubemap（256²×6，脏标记 + 节流，排太阳圆盘）；SH-L2 漫反射；GGX 预滤波 + DFG LUT；不透明/透明环境项替换为 IBL；`ibl_enabled`；`sky_uniform` 常量天空自检开关 | ✅ 收官（2026-11-23，含 9.2.8 现场复检补修；[段 README](9.2-天空盒IBL/README.md)、[判定线](9.2-天空盒IBL/9.2.7-判定线与计量.md)） |
| 9.3 | [9.3-GlobalProbe方向桶](9.3-GlobalProbe方向桶/README.md) | 16 桶 × RGBL1（4×4 八面体，32 样本/桶）；法线选桶求值；`ibl_mode` L2/16 桶 A/B；为 9.7 体积外兜底建格式 | 📝 施工计划已立案并按 9.2 落地复核（[计划](9.3-GlobalProbe方向桶/施工计划：GlobalProbe方向桶.md)，未开工；2026-11-23） |
| 9.4 | 9.4-求交器与射线基建 | 硬件 ray query 主后端 + 可替换接口；BLAS 挂驻留资产、TLAS 每帧从实例表构建；命中点属性重建 | ⬜ 未开工 |
| 9.5 | 9.5-patch池与分配回收 | Volume + 级联格子 + 16 方向桶 + 定长 ring；屏幕播种/复用/滚动/心跳/整理；rank 语义；motion vectors 前置件 | ⬜ 未开工 |
| 9.6 | 9.6-irradiance更新 | 每活跃 patch 半球采样；方向/点光/自发光直接光与缓存自查询多跳；L1 投影 + 方差引导时域混合；bounce 分配 | ⬜ 未开工 |
| 9.7 | 9.7-屏幕gather与注入 | 1/4 查表（rank 0）+ 空间/时域滤波 + 去噪 + 双边上采样 + L1 压缩；Volume 内替换 IBL 漫反射，体积外保持 9.3 的漫反射编码（同属 IBL 基线） | ⬜ 未开工 |
| 9.8 | 9.8-动态验收与压测 | 动态几何/动态光照（含 TOD 天空）收敛行为；Volume 边界与兜底；成组压测；已知局限逐项自检 | ⬜ 未开工 |

一次只施工一段：9.1 天空 → 9.2 IBL → 9.3 GlobalProbe → 9.4 求交器 → 9.5 建池 → 9.6 出辐照度 → 9.7 接屏幕 → 9.8 收口。**顺序不可倒**：IBL 是 SCGI 的对照基线，求交器是缓存的前置；9.4 与 9.3 可并行，但 9.7 必须等 9.2/9.3/9.5/9.6 全部就位。逐段独立验收以[路线图步骤 9 范围拆解表](../学习目标实现步骤.md)的验收重点列为准；头号待决项见[施工计划 §3](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)。

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
- 设备能力：本机 RTX 2060 已核实支持 `VK_KHR_acceleration_structure` / `VK_KHR_ray_query` / `VK_KHR_ray_tracing_pipeline` / `VK_KHR_ray_tracing_position_fetch`；**9.1～9.3 不依赖 RT**，9.4 起才要求。不支持 RT 时 GI 走关闭路径（9.4 feature 门）。
- 验收场：Sponza（室内基座/漏光）、zero_day（室外天空与规模）、DamagedHelmet（金属/粗糙度高光）、大世界档（Size 512）；Unity 251 场景与 Atrio 只作质量参照。
- 目录组织：天空/IBL 落 `render/passes/sky/`、`render/passes/ibl/`；GI 代码落 `render/rt/`、`render/resident/tables/surface_cache.rs`、`render/passes/gi/`；shader 落 `shaders/common/{sky,ibl,octahedral}.wgsl`、`shaders/scene/{ibl,gi}/`（[施工计划 §5](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)）。

## 材料清单

- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——选型结论（含天空/IBL §1.5）、八段拆解、参数基线、待决项与边界（2026-11-20 立案/重规划）。
- [9.1 段任务面板](9.1-程序化天空盒/README.md)与[施工计划：程序化天空盒](9.1-程序化天空盒/施工计划：程序化天空盒.md)——天空模型、UBO 布局与判定线；[功能流程导读](9.1-程序化天空盒/程序化天空：从参数到背景像素.md)（2026-11-23 收官）。
- [9.2 段任务面板](9.2-天空盒IBL/README.md)与[施工计划：天空盒IBL](9.2-天空盒IBL/施工计划：天空盒IBL.md)——捕获、SH-L2、预滤波、DFG LUT、注入与描述符布局。
- [9.3 段任务面板](9.3-GlobalProbe方向桶/README.md)与[施工计划：GlobalProbe方向桶](9.3-GlobalProbe方向桶/施工计划：GlobalProbe方向桶.md)——八面体映射、16 桶 L1、A/B 口径与兜底格式（2026-11-23 按 9.2 落地复核）。
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
