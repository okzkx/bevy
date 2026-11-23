# 9.3 GlobalProbe 方向桶（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：9.2 的漫反射是单个 SH-L2（空间不变、方向连续）。本段从同一张天空 cubemap 再生成一份 **16 方向桶 × RGBL1** 的全局探针：4×4 八面体分桶、每桶半球采样 32 方向、投影成 L1 并余弦卷积。它有两个用途：一是与 9.4 之后的 patch 数据格式同构，给 9.7 的屏幕 gather 提供体积外兜底；二是作为 L2 之外的另一种方向类别 LUT，用 A/B 决定默认。

**一句话说明**：从 9.2 的天空 cubemap 生成 16 桶 × RGBL1 的 GlobalProbe，按法线选桶求值，并用 A/B 对照 L2；本段不产生空间缓存、不发射射线、不替代 L2 作为默认（除非数据支持）。

**当前状态：✅ 收官（2026-11-23）。** 任务 9.3.1～9.3.5 全部落地，判定线 7 条全过（[9.3.5 判定线与计量](9.3.5-判定线与计量.md)）；A/B 结论 = **默认保留 SH-L2**（16 桶无量化收益也无伪影），16 桶作为 9.7 的 Volume 外兜底编码。功能流程走读见[GlobalProbe 方向桶流程](GlobalProbe方向桶流程.md)。

## 代码落点

- `shaders/common/octahedral.wgsl`（新）：八面体球↔方映射、方向↔桶索引、桶中心方向；9.5/9.7 的 cell/patch 桶位共用同一份。
- `shaders/scene/ibl/global_probe.wgsl`（新）：16 桶 × 32 半球样本的 L1 投影与余弦卷积；写 16×4 vec4（1KB）。
- `shaders/common/ibl.wgsl`：`probe16_eval`（值参数：调用方按桶索引传 4 个 vec4）与 `octahedral` 导入。
- `shaders/scene/gbuffer/gbuffer_composite.wgsl`、`shaders/scene/transparent/transparent_forward.wgsl`：`ibl_mode` 分支、IBL 集 b4、mode 11 桶可视化（仅不透明）。
- `src/render/passes/ibl/octahedral.rs`（新）：映射/求值的 CPU 镜像与单测（`ibl_stats` 摘要复用）。
- `src/render/passes/ibl/environment.rs`：probe buffer（1KB，`BufferRole::DeviceStorage`）、投影管线与捕获侧 set、帧集 b4、初始化清零、烘焙链 dispatch 与屏障。
- `src/remote/features/ibl.rs`：`ibl_set` 的 `diffuse_mode`，`ibl_stats` 的 `probe16` 摘要，`ibl_census` 的 `probe: true` 全量读回。
- `src/render/frame/render_mode.rs`：`probe_buckets` 调试档（mode 11）。
- `src/remote/census/frame_dump.rs`（新）+ `src/render/passes/post/smaa.rs`：`frame_dump` 帧取证（LDR → PPM，锁屏环境兜底）。

## 任务面板

| 任务 | 交付 | 状态 |
|---|---|---|
| 9.3.1 八面体公共片段 | `octahedral.wgsl` + CPU 镜像；正向索引 ×res 截断（Unity 同式）；4 条单测 | ✅ [记录](9.3.1-八面体公共片段与CPU镜像.md) |
| 9.3.2 16 桶 L1 投影 | `global_probe.wgsl`（一桶一线程、32 分层中点半球样本、`(2π/N)·A·Y²` 折常数）；probe buffer/集/管线接入烘焙链 | ✅ [记录](9.3.2-16桶L1投影与烘焙链接入.md) |
| 9.3.3 查询与模式切换 | `ibl_mode` 0 = SH-L2、1 = GlobalProbe；composite/transparent 两处分支，共用 9.2.8 翻转法线；帧集 b4 | ✅ [记录](9.3.3-查询与ibl_mode分支.md) |
| 9.3.4 BRP 与调试视图 | `ibl_set.diffuse_mode`；`ibl_stats.probe16`（六方向 + 桶中心能量）；`ibl_census.probe`；mode 11；`frame_dump` 取证口 | ✅ [记录](9.3.4-BRP与调试视图.md) |
| 9.3.5 判定线与 A/B | 七条判定线逐条取证、A/B 结论、五脚本回归与工程检查 | ✅ [记录](9.3.5-判定线与计量.md) |

## 材料清单

- [施工计划：GlobalProbe方向桶](施工计划：GlobalProbe方向桶.md)——算法、映射约定、A/B 口径、任务拆分、判定线、测试方案与坑（2026-11-20 立案；2026-11-23 同步修订与落地复核）。
- [GlobalProbe 方向桶流程](GlobalProbe方向桶流程.md)——从捕获到着色求值的全流程走读与代码改动解析（2026-11-23 收官）。
- [9.3.1](9.3.1-八面体公共片段与CPU镜像.md) / [9.3.2](9.3.2-16桶L1投影与烘焙链接入.md) / [9.3.3](9.3.3-查询与ibl_mode分支.md) / [9.3.4](9.3.4-BRP与调试视图.md) / [9.3.5](9.3.5-判定线与计量.md) 施工记录。
- [9.2 段 README](../9.2-天空盒IBL/README.md)、[9.2.7 判定线与计量](../9.2-天空盒IBL/9.2.7-判定线与计量.md)与[9.2.8 现场复检补修](../9.2-天空盒IBL/9.2.8-高光火花、天空地平线噪点与天光开关联动.md)——本段的落地基线与回归口径（IBL 默认关、天光联动、视向侧法线）。
- [9.2 施工计划 §0.9](../9.2-天空盒IBL/施工计划：天空盒IBL.md)——`sky_uniform` 常量天空开关（9.3 的能量自检依赖它）。
- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§2 9.3 节与 §5 目录组织。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——GlobalProbe 16 桶与 Cell 16 槽的区别、16 桶 vs L2 的对照。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——`GlobalProbe.compute` / `GlobalProbe.hlsl` 的源码级依据。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——`DistanceFallback` 的定位与质量档。
