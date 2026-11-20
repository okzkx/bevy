# 步骤 9：表面缓存全局光照（M8）

对应[执行路线图](../学习目标实现步骤.md)。本 README 是步骤索引与材料目录；各段任务面板在段目录 README（开工时立），段施工计划随段立案入段目录。

**目的**：步骤 8 已把管线补成完整延迟管线，但间接光仍是一个常量环境项。本步补上运行时动态 diffuse GI：在步骤 8 的 G-buffer 与光照累积地基上，按 Unity SCGI 范式做世界空间表面缓存——把稀缺射线花在贴表面的缓存 patch 上、多跳靠缓存自查询、屏幕侧 1/4 分辨率 gather 后注入光照累积 pass。

**一句话说明**：用"空间格子 + 方向桶 + 定长 patch 池"缓存世界空间的表面辐照度，少量射线维护缓存、缓存自查询做多跳，屏幕侧查缓存→滤波→上采样→注入光照累积；本步只主张间接漫反射，不主张镜面反射、AO 与烘焙。

**当前状态：施工计划已立案（2026-11-20，[施工计划：表面缓存全局光照六段拆解](施工计划：表面缓存全局光照六段拆解.md)），选型已拍板，六段均未开工。** 选型结论=**Unity SCGI 骨架 + Kajiya 三件借用**：空间格子 + 16 方向桶 + 定长 65536 patch 池 + 屏幕播种 + rank/heartbeat/defrag；借 Kajiya 的法线偏置、rank 不变式、覆盖不足时的投票分配器（二期备件）。理由与推翻判据见施工计划 §1。

## 施工顺序

| 段 | 文件夹（开工时立） | 范围 | 状态 |
|---|---|---|---|
| 9.1 | [9.1-间接光注入与SH查询](9.1-间接光注入与SH查询/README.md) | 谱系档 2、全步公共接口、零射线起步：全局 SH 探针接入光照累积间接项；SH L1 余弦卷积；GI 开关与计量 | 📝 施工计划已立案（[计划](9.1-间接光注入与SH查询/施工计划：间接光注入与SH查询.md)，未开工） |
| 9.2 | 9.2-求交器与射线基建 | 硬件 ray query 主后端 + 可替换接口；BLAS 挂驻留资产、TLAS 每帧从实例表构建；命中点属性重建 | ⬜ 未开工 |
| 9.3 | 9.3-patch池与分配回收 | Volume + 级联格子 + 16 方向桶 + 定长 ring；屏幕播种/复用/滚动/心跳/整理；rank 语义；motion vectors 前置件 | ⬜ 未开工 |
| 9.4 | 9.4-irradiance更新 | 每活跃 patch 半球采样；方向/点光/自发光直接光与缓存自查询多跳；L1 投影 + 方差引导时域混合；bounce 分配 | ⬜ 未开工 |
| 9.5 | 9.5-屏幕gather与注入 | 1/4 查表（rank 0）+ 空间/时域滤波 + 去噪 + 双边上采样 + L1 压缩；替换 9.1 探针实现 | ⬜ 未开工 |
| 9.6 | 9.6-动态验收与压测 | 动态几何/动态光照收敛行为；Volume 边界与兜底；成组压测；已知局限逐项自检 | ⬜ 未开工 |

一次只施工一段：9.1 是接口与着色集成的地基；9.2 是求交器（与缓存解耦，可独立验收）；9.3 建池；9.4 出辐照度；9.5 接屏幕；9.6 收口。**9.1 → 9.2 → 9.3 → 9.4 → 9.5 顺序不可倒**，与谱系调研"查询端 → 求交器 → 动态缓存"一致。逐段独立验收以[路线图步骤 9 范围拆解表](../学习目标实现步骤.md)的验收重点列为准；头号待决项见[施工计划 §3](施工计划：表面缓存全局光照六段拆解.md)。

## 参数基线（6GB 显存账）

| 阶段 | Res | 级联 | Size | 最细体素 | 覆盖 | cell 表（单张 / 双表） | patch 池 |
|---|---|---|---|---|---|---|---|
| 9.3 起步 | 32 | 4 | 128 m | 0.5 m | 128 m | 8 MiB / 16 MiB | 65536 条 ≈ 12 MiB |
| 大世界档 | 32 | 6 | 512 m | 0.5 m | 512 m | 12 MiB / 24 MiB | 同上（可提 2^18 ≈ 48 MiB） |
| 高品质候选（二期） | 64 | 4 | 128 m | 0.25 m | 128 m | 64 MiB / 128 MiB；按级联降桶 16/8/4/4 → 32 MiB / 64 MiB | 同上 |

覆盖靠加级联（线性内存）而不是加分辨率；一期就用满 16 桶以对齐 Unity 源码。完整推导与显存口径见[施工计划 §1.3](施工计划：表面缓存全局光照六段拆解.md)。

## 开工前置

- 步骤 8 已收官（8.1～8.6）；步骤 5/6 的步骤级验收归档欠账不阻塞本步。
- F25/F26 挂账不阻塞本步（[待修复清单](../待修复清单/待修复清单.md)）。
- 设备能力：本机 RTX 2060 已核实支持 `VK_KHR_acceleration_structure` / `VK_KHR_ray_query` / `VK_KHR_ray_tracing_pipeline` / `VK_KHR_ray_tracing_position_fetch`；不支持 RT 时 GI 走关闭路径（9.2 feature 门）。
- 验收场：Sponza（长期基座）、zero_day（规模）、大世界档（Size 512）三个口径；Unity 251 场景与 Atrio 只作质量参照。
- 目录组织：沿用步骤 8 的 `render/` 归属，GI 代码落 `render/rt/`、`render/resident/tables/surface_cache.rs`、`render/passes/gi/`；shader 落 `shaders/scene/gi/` 与 `shaders/common/gi_*.wgsl`（[施工计划 §5](施工计划：表面缓存全局光照六段拆解.md)）。

## 材料清单

- [施工计划：表面缓存全局光照六段拆解](施工计划：表面缓存全局光照六段拆解.md)——选型结论、六段拆解、参数基线、待决项与边界（2026-11-20 立案）。
- [9.1 段任务面板](9.1-间接光注入与SH查询/README.md)与[施工计划：间接光注入与SH查询](9.1-间接光注入与SH查询/施工计划：间接光注入与SH查询.md)——9.1 段级任务拆解、SH 口径（折叠基函数的辐照度多项式）、帧 UBO 布局与判定线（2026-11-20 立案）。
- [Unity SCGI 调研](../../笔记/光照与材质/Unity%20SCGI%20调研.md)——发布状态、运行模型、参数面、已知局限、测试资源与调研来源。
- [Unity SCGI 实现](../../笔记/光照与材质/Unity%20SCGI%20实现.md)——源码级数据流：Volume/cell/patch 池、分配与回收、采样、四级滤波、Kajiya 对照与收益代价。
- [全局光照谱系调研](../../笔记/光照与材质/全局光照谱系调研.md)——四族谱系、选型阶梯与"查询端 → 求交器 → 动态缓存"的顺序约束。
- 本机源码位置（2026-11-20 核实；执行方式=可模仿/照抄，许可边界见[施工计划 §8.1](施工计划：表面缓存全局光照六段拆解.md)）：
  - Unity core：`F:\Program Files\Unity\7000.0.0a9\Editor\Data\Resources\PackageManager\BuiltInPackages\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\`（`SurfaceCache.cs`、`SurfaceCacheWorld.cs` 与各 compute/hlsl）；
  - Unity URP：`...\com.unity.render-pipelines.universal\Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\`（`SurfaceCacheGIRendererFeature.cs`、`SurfaceCacheWorldAdapter.cs`、`ScreenResolve*`）；
  - Unity 实跑工程：`F:\okzkx\TestSCGI`、`F:\okzkx\TestUnity67`、`F:\okzkx\Unity-Atrio-Demo`；
  - Kajiya：本机未检出；执行前 `git clone --depth 1 https://github.com/EmbarkStudios/kajiya F:\okzkx\kajiya`（`docs/gi-overview.md`、`crates/lib/kajiya/src/renderers/ircache.rs`、`assets/shaders/ircache/`）。
- [Unity 6.7 手册：Surface Cache Global Illumination in URP](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi.html)——启用步骤、质量调优、自定义 Shader、排障与 Volume 参考。
- [官方 251 场景：251_SurfaceCache_URP_GI.unity](https://github.com/Unity-Technologies/Graphics/blob/6000.7/staging/Tests/SRPTests/Projects/UniversalGraphicsTest_Lighting/Assets/Scenes/251_SurfaceCache_URP_GI.unity)——受控 A/B 参照（Size 20 / Res 128 / 1 级联 / 32 样本）。
- [Atrio 官方 Demo](https://github.com/Unity-Technologies/Unity-Atrio-Demo)——Unity 侧画质参照（无 lightmap，SCGI 主打）；[社区 Cornell Box](https://github.com/Arlorean/UnitySCGI-CornellBox)——最小复现场。
- [8.1 G-buffer 段](../8-完整延迟渲染管线/8.1-G-buffer与不透明几何/)——G-buffer 三原料与 motion vectors 附件位（9.3 前置件）。
- [8.3 CSM 段](../8-完整延迟渲染管线/8.3-CSM与PCSS级联阴影/)——9.4 直接光遮挡复用的阴影链。
