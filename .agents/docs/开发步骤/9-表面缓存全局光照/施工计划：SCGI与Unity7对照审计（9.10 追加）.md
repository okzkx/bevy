# 施工计划：9.10 SCGI 与 Unity7 URP 对照审计

> 2026-11-26 立案（纯文档，未开工；2026-11-26 用户增补段，编号追加在 9.8 之后）。对应[步骤 9 README](README.md)、[八段拆解 §8 对照索引](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../学习目标实现步骤.md)。对照基准是本机 URP 17.7.0 源码：core `...\com.unity.render-pipelines.core@d1e054958f77\Runtime\Lighting\SurfaceCache\`（26 个非 meta 文件）、URP `...\com.unity.render-pipelines.universal@f74a61fa3847\Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\`（14 个非 meta 文件）；许可边界与"可逐行搬运逻辑"的授权见[八段 §8.1](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)，机制整理见[Unity SCGI 实现](../../笔记/光照与材质/Unity%20SCGI%20实现.md)，Unity 侧发布状态见[Unity SCGI 调研](../../笔记/光照与材质/Unity%20SCGI%20调研.md)。本文只管 9.10 段做什么、按什么顺序、验收什么；开工后在该段目录（`9.10-SCGI与Unity7对照审计/`，开工时立）落对照表、审计报告与施工记录。
>
> **当前状态：⬜ 已立案（2026-11-26 用户增补），未开工。** 任务 9.10.1～9.10.6 为待施工拆解；前置闸门见 §3、判定线草案见文末。

**一句话说明**：把本仓库步骤 9 里与 SCGI 相关的代码（9.3–9.7 全量 + 9.1/9.2 被 SCGI 消费的那一小块）与 Unity URP 17.7 的 40 个源文件逐文件、逐 kernel 对照，产出带证据的"缺漏/冗余/实现错误"清单，并把 P0 小修当场闭合、把结构性缺口登记为后续立案——本段不做 1:1 移植，不改 9.8 的范围，不对 Unity 的天空/IBL 模型本身做品评。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-26 情况核实清单

| 项 | 结论 | 依据 |
|---|---|---|
| 对照面大小 | Unity 40 个非 meta 文件、9 738 行（core 4 822 + URP 4 916）；其中 2 个是程序集元数据、1 个跨段 | 本机包路径 `ls` + `wc -l`（2026-11-26 实测） |
| 本仓库被测面 | GI 侧 14 个 shader（`ash_renderer/shaders/scene/gi/`）+ 8 个公共片段（`shaders/common/gi_*.wgsl`）+ 7 个 pass 模块（`src/render/passes/gi/*.rs`）+ 求交/常驻资源（`render/ray_tracing/`、`resident/`）+ BRP（`remote/features/gi.rs`、`render_settings.rs`）；量级约 5.4k 行 | 目录实测 |
| 既有映射索引 | 八段 §8 有 12 行"Unity 文件 ↔ 本工程落点"的立案期索引，本段要校正并补全到 40 行 | `施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md:291-308` |
| 既有回归工具 | `tools/gi_*.py` 共 14 个：gi_test、gi_energy_regression、gi_estimation_test、gi_gather_test、gi_flicker_test、gi_flicker_isolate、gi_flicker_map、gi_geometry_jitter、gi_pixel_audit、gi_point_audit、gi_probe_regression、gi_resize_regression、gi_scene_regression、gi_stage_audit | `tools/` 实测 |
| 已知顺延项 | 调试视图 16/17 未实现（`remote/features/gi.rs:135` 的 `>15` 截断）、无 `scgi_ms`（源码 grep 零命中）、`GI_COUNTER_WORDS` 16→20 未扩 | 9.7 段施工计划/README、路线图 9.7 行 |
| 9.7 判定线遗留 | 透明 A/B 读图缺、rank1 取证缺、Volume 边界环/振铃未留档、漏光案例未做（八段 §6 说归 9.8） | 9.7 段施工计划判定线、路线图 `:267` |
| 9.9 工具 | 9.9 立案的颜色面板 / `color_scope_get` 是审计修复的读数工具（9.9 未开工时用 `frame_dump` + 现有 BRP） | [9.9 施工计划](施工计划：ToneMapping颜色校验面板（9.9 追加）.md) |
| 9.8 状态 | 未立项未开工，不在对照面 | 路线图 `:268` |

### 0.1 判定口径：按功能语义，不按文件一一对应

- 一一对应只做**线索**，结论按功能。Unity 的 `ObjectDispatcherChangeSource.cs`/Entity 桥是引擎侧机制，本仓库的等价物是 Bevy 资产事件 + 场景变更集，**不算缺漏**；`FallbackMaterial.mat/.shader` 是 Unity 材质资产，本仓库的等价物是合成 pass 的 IBL 回退分支，**不算缺漏**。
- **缺漏** = 功能语义缺失且影响本步主张（Volume 内不透明 diffuse 注入：池/滤波/压缩/屏幕解析/调试/BRP）。
- **冗余** = 两边都有的能力在本仓库重复实现，或无调用方（含未接线的调试视图、BRP 字段、生成的 shader 入口）。
- **实现错误** = 语义偏差且能给出可观测后果（能量偏差、漏光、闪烁/滞后、过量带宽或原子）。
- 一律用"是否影响本步主张"裁决，不用"与 Unity 是否逐行一致"裁决；机制差异写"机制不同，等价物在 X"。

### 0.2 三张清单的判定示例

- 缺漏（已知待核）：`Debug.compute` 的调试视图枚举比本仓库多，`gi.rs:135` 把 `view > 15` 截断，视图 16/17 未实现。
- 冗余（待核）：同一参数在 9.5/9.6 两处 UBO 重复打包；`OUT_DIR/shader_abi.rs` 里有入口但没有任何 `PipelineDesc` 引用（用 ABI 表与管线声明交叉核）。
- 实现错误（待核）：法线偏置、rank 过滤、L1 归一化、压缩量化若与 Unity 语义不同，会表现为能量偏差或边缘漏光——每条必须有 BRP 读数或读图证据，不接受"看起来不对"。

### 0.3 分级与处置

- **P0 必修**：本段当场修。单处 ≤ 约 80 行、不改架构；改完跑对应回归与读图。
- **P1 记档另立**：结构性（视图 16/17、`scgi_ms`、计数扩容、TLAS 每帧全量重建、AS compaction、压缩格式变更），写清"为什么不在本段修"。
- **P2 不做**：Unity 编辑器/平台专用、与本步主张无关，写明理由。
- **待拍板**：涉及 9.8 范围或默认参数（AO 是否乘 IBL、alpha 裁切、9.7 默认参数）。
- 每条必须有"处置 + 归属 + 证据"三列，不允许只写"存在差异"。

### 0.4 边界（本段不主张什么）

- 不做 1:1 移植，不把 Unity 代码/资产搬进仓库；照抄逻辑时用 WGSL 重写并注明来源（八段 §8.1 的授权范围内）。
- 不评 Unity 的天空模型与 IBL BRDF/DFG（本仓库 9.1/9.2 另有 Filament 对照）；只看 SCGI 消费环境 cubemap/太阳参数的那一小块接驳。
- 不做大规模重构：不动 9.4 的求交后端选择、不改 9.5 的池布局与 ring 深度（P0 小修除外）。
- 不替 9.8 跑长时压测与稳定性结论（只取审计所需短时读数）；不改 9.8 的范围。
- 不为"对齐 Unity"牺牲 GPU-driven 闭环与 6GB 预算；冲突处按八段 §1.3 的参数基线裁决并记档。
- 不把"Unity 有而本仓库没有"直接写成缺陷：先按 §0.1 判断是否属于引擎机制差异。

## 1. 对照面清单（40 文件 → 本仓库落点 → 段）

说明：落点列是立案期估计；标"待核"的正是 9.10.1 要落成实落点的行。八段 §8 的 12 行索引由本表补全（方式见 §5 待决六）。

### 1.1 Unity core（`Runtime/Lighting/SurfaceCache`，26 文件）

| Unity 文件 | 本仓库落点（估计） | 段 | 立案期状态 |
|---|---|---|---|
| `AssemblyInfo.cs` | —（程序集元数据） | 不适用 | 不适用 |
| `Common.hlsl` | `shaders/common/gi_cell.wgsl`（待核 `gi_lookup.wgsl`） | 9.5 | 待核 |
| `Defrag.compute` | `shaders/scene/gi/defrag.wgsl` + `passes/gi/allocation.rs` | 9.5 | 已知 |
| `EmissiveTriangleAddition.compute` | `shaders/scene/gi/emissive_build.wgsl` + `passes/gi/estimation.rs` | 9.6 | 已知 |
| `EmissiveTriangleRemoval.compute` | `emissive_build.wgsl` 的移除分支 | 9.6 | 待核 |
| `EmissiveTriangles.hlsl` | `emissive_build.wgsl` 结构 + `shaders/common/gi_emissive.wgsl` | 9.6 | 待核 |
| `Estimation.compute` | `shaders/scene/gi/estimation.wgsl` + `passes/gi/estimation.rs` | 9.6 | 已知 |
| `Eviction.compute` | `shaders/scene/gi/eviction.wgsl` + `passes/gi/allocation.rs` | 9.5 | 已知 |
| `GlobalProbe.compute` | `shaders/scene/ibl/global_probe.wgsl` + 9.3 的 pass | 9.3 | 已知 |
| `GlobalProbe.hlsl` | `global_probe.wgsl` 的公共结构 | 9.3 | 待核 |
| `IrradianceCompression.hlsl` | `shaders/scene/gi/upsample.wgsl` + `shaders/common/gi_compression.wgsl` | 9.7 | 已知 |
| `PatchAllocation.compute` | `shaders/scene/gi/patch_alloc.wgsl` + `passes/gi/allocation.rs` | 9.5 | 已知 |
| `PatchAllocationRequest.hlsl` | `shaders/common/gi_bounce.wgsl`（bounce 分配请求） | 9.6 | 待核 |
| `PatchUtil.hlsl` | `shaders/common/gi_cell.wgsl` / `gi_ring.wgsl` / `gi_lookup.wgsl` 的公共工具 | 9.5 | 待核 |
| `PathTracing.hlsl` | `estimation.wgsl` 的求交循环 + `shaders/common/gi_sampling.wgsl` | 9.6 | 待核 |
| `PunctualLightSampling.compute` | `shaders/scene/gi/light_sampling.wgsl` + `passes/gi/estimation.rs` | 9.6 | 已知 |
| `PunctualLights.hlsl` | `shaders/common/gi_light_sample.wgsl` | 9.6 | 已知 |
| `RingBuffer.hlsl` | `shaders/common/gi_ring.wgsl` | 9.5 | 已知 |
| `Scrolling.compute` | `shaders/scene/gi/scrolling.wgsl` + `passes/gi/volume.rs` | 9.5 | 已知 |
| `SpatialFiltering.compute` | `shaders/scene/gi/spatial_filter.wgsl` + `passes/gi/filtering.rs` | 9.7 | 已知 |
| `SurfaceCache.cs` | 无单点对应：录制顺序在本仓库的 `passes/gi/mod.rs` | 跨段 | 待核 |
| `SurfaceCacheRenderPipelineResources.cs` | 资源声明/描述符布局（待核） | 9.5 | 待核 |
| `SurfaceCacheWorld.cs` | 场景接驳与变更集（`passes/gi/volume.rs`、`resident/` 等价） | 9.4 | 已知 |
| `TemporalFiltering.compute` | `shaders/scene/gi/temporal_filter.wgsl` + `passes/gi/filtering.rs` | 9.7 | 已知 |
| `Unity.SurfaceCache.Runtime.asmdef` | —（程序集元数据） | 不适用 | 不适用 |
| `VectorLogic.hlsl` | `shaders/common/gi_cell.wgsl` 的工具（待核） | 9.5 | 待核 |

### 1.2 Unity URP（`Runtime/RendererFeatures/SurfaceCacheGIRendererFeature`，14 文件）

| Unity 文件 | 本仓库落点（估计） | 段 | 立案期状态 |
|---|---|---|---|
| `Debug.compute` | `shaders/scene/gi/patch_view.wgsl` + `passes/gi/view.rs`（视图 16/17 未实现） | 9.7 | 待核 |
| `FallbackMaterial.mat` | 合成 pass 的 IBL 回退分支（`gbuffer_composite` 等价） | 9.7 | 待核 |
| `FallbackMaterial.shader` | 同上 | 9.7 | 待核 |
| `FlatNormalResolution.compute` | `screen_gather.wgsl` 的法线来源（待核） | 9.7 | 待核 |
| `ObjectDispatcherChangeSource.cs` | 无对应：Bevy 资产事件 + 场景变更集 | 9.4 | 机制不同 |
| `PatchAllocation.compute` | `patch_alloc.wgsl` 的 URP 变体（待核差异） | 9.5 | 待核 |
| `ScreenResolveDenoising.compute` | `shaders/scene/gi/denoise.wgsl` + `passes/gi/screen_gather.rs` | 9.7 | 已知 |
| `ScreenResolveLookup.compute` | `shaders/scene/gi/screen_gather.wgsl` | 9.7 | 已知 |
| `ScreenResolveUpsampling.compute` | `shaders/scene/gi/upsample.wgsl` | 9.7 | 已知 |
| `SurfaceCacheEntityChangeBridge.cs` | 无对应：Bevy 场景变更集 | 9.4 | 机制不同 |
| `SurfaceCacheEntityRecords.cs` | `passes/gi/volume.rs` 的实体/体积记录（待核） | 9.4 | 待核 |
| `SurfaceCacheGIRendererFeature.cs` | `passes/gi/mod.rs` 的录制顺序与 BRP `remote/features/gi.rs`（接驳部分归 9.4） | 9.7 | 待核 |
| `SurfaceCacheWorldAdapter.cs` | `passes/gi/volume.rs` 的场景适配层 | 9.4 | 待核 |
| `SurfaceCacheWorldChangeSet.cs` | 场景变更集（`passes/gi/volume.rs`） | 9.4 | 待核 |

### 1.3 归属统计（立案期估计）

| 归属 | 文件数 | 说明 |
|---|---|---|
| 9.3 GlobalProbe | 2 | core 的 `.compute` + `.hlsl` |
| 9.4 场景接驳 | 6 | core `SurfaceCacheWorld.cs` + URP 5 个 World/Entity/Dispatcher 文件；其中 2 个是机制不同 |
| 9.5 patch 池 | 10 | core 9 个（含待核 4）+ URP `PatchAllocation.compute` |
| 9.6 估计/光源/自发光 | 8 | 全在 core：自发光 3 + 估计 2 + 光源 2 + bounce 分配 1 |
| 9.7 屏幕侧/压缩/合成/调试图 | 11 | core 3 + URP 8 |
| 跨段 | 1 | `SurfaceCache.cs`（录制顺序） |
| 不适用 | 2 | `AssemblyInfo.cs`、`asmdef` |
| **合计** | **40** | 与 §1.1+§1.2 行数一致 |

本仓库被测面（自检锚点）：14 个 `shaders/scene/gi/*.wgsl`、8 个 `shaders/common/gi_*.wgsl`、7 个 `passes/gi/*.rs`、`shaders/scene/ibl/global_probe.wgsl`、`remote/features/gi.rs`、14 个 `tools/gi_*.py`、`9.7` 的调试视图与 BRP。

## 2. 任务拆解

### 9.10.1 对照面建表：把 40 行从"估计"落成"实落点"

- 逐文件读 Unity 源码，列出每个 compute 的 kernel 名与每个 hlsl 导出的函数，与本仓库 shader 的入口/函数做双向索引；产出 `9.10-SCGI与Unity7对照审计/对照表.md`（主表：Unity 文件/kernel → 本仓库文件/入口 → 段 → 状态）。
- 把八段 §8 的 12 行旧索引校正/补全为 40 行，落点与 §1 的估计逐行核对，**修正估计错误的行**并记下原因。
- 验收：40 行无"未审"；每个 kernel 有归属；"待核"清零或转为明确的发现条目。

### 9.10.2 逐文件读码审计（三分类 + 证据 + 分级）

- 按 §1 的表逐文件对照，重点维度：数据布局（L0/L1、rank、heartbeat、反向 cell、ring 指针）、采样与积分（半球/余弦、方差引导、压缩量化）、滤波（taps、切平面、时域权重与滞后）、屏幕解析（1/4 查找、双边上采样、去噪）、分配/回收（播种、滚动、心跳、整理、bounce 分配）、场景接驳（变更集、材质分类、法线）、调试与 BRP。
- 每条发现记：现象、证据（Unity `file:line` ↔ 本仓库 `file:line`）、影响面（画面/性能/可维护性）、分级（§0.3）、处置。
- 先核销 9.7 已知顺延 6 项（视图 16/17、`scgi_ms`、计数 16→20、透明 A/B 读图、rank1 取证、Volume 边界环/振铃），逐项给"本段修 / 另立 / 不做"的结论。

### 9.10.3 参数与常量对照（≥20 项）

- 表：参数 | Unity 值 | 本仓库值 | 是否一致 | 不一致理由 | 证据。
- 覆盖：patch 池容量与 rank 上限、cell 尺寸/cascade/桶数、采样档与预热、滤波权重与滞后、L1 压缩格式与量化、ring 深度、bounce 预算、屏幕解析降采样率、调试视图枚举、曝光/tonemap 常量（与 9.7.9、8.6 交叉）、9.9 新增的 `tonemap_strength`（默认 1.0）。
- 验收：≥20 项，每个"不一致"都有理由或转成发现条目。

### 9.10.4 运行时取证

- 三组场景：默认场景、薄墙/漏光案例、室内外过渡；每组 ≥1 组 BRP 读数（`gi_stats`/`exposure_get`，9.9 就位则加 `color_scope_get`）+ ≥1 张读图；收敛/滞后按秒级记录。
- 14 个 `tools/gi_*.py` 先跑一轮基线（环境/端口按测试规范），失败项记录退出码与日志位置，**不算本段缺陷**（除与本次发现直接相关）。
- 验收：证据可复现（命令、脚本、场景、机位写进报告）；读数不只贴结论，要贴改前后对照。

### 9.10.5 P0 小修闭环

- 只做 §0.3 的 P0：每项按"现象 → 根因 → 最小改法 → 改前后读数/读图 → 回归脚本结果"记录；单处 ≤ 约 80 行、不改架构。
- 若 P0 超出预算（>3 处结构性），停在报告里、把超出项转 P1 另立，不硬塞。
- 验收：P0 全闭或全转档；零新 VUID、clippy 全净、无新依赖；改动相关回归全过。

### 9.10.6 审计报告与登记

- 报告落 `9.10-SCGI与Unity7对照审计/审计报告.md`：摘要（P0/P1/P2 计数）、三张清单、参数对照、运行时证据、复跑命令、局限。
- 结构性项写成立案建议（段号/范围/验收草案一句话），由用户拍板后登记；文档侧同步：README/路线图段表状态、9.7 状态失真处、八段 §8 索引补全。
- 验收：报告可独立阅读；每条结论能追到证据；不改写历史记录。

## 3. 前置闸门（开工首日按序关闭）

① 版本钉死：记录 Unity 包路径、40 文件字节数与行数、本仓库 HEAD；审计期间不升级包、不改 9.4–9.7（P0 除外）。
② 脚本基线：14 个 `gi_*.py` 先跑一轮，记录环境/端口/失败项，避免把环境问题当归因。
③ 读图通道：截图方式与命名规则；截图前把 overlay 关到只留必要页（或固定机位），临时文件按项目规矩落盘。
④ 读数通道：9.9 就位则用面板/BRP；否则用 `frame_dump` PPM + 现有 BRP 读数。
⑤ 改动预告：开工前把预计会动的文件列清单（`shaders/scene/gi/*.wgsl`、`passes/gi/*.rs`、`remote/features/gi.rs` 等），防止边审边改失控。

## 4. 显存与性能口径

- 审计本身不新增常驻资源；P0 若新增缓冲/纹理须单列显存并计入 6GB 账。
- 运行时取证短时（≤30 s/组），不跑 9.8 的长时压测；不出帧率稳定性结论。
- 每次 P0 记录前后帧时间/`draw_calls`/GI 读数；性能类发现必须给"可复现的测量方式"。

## 5. 待决项（施工时钉明，不预设结论）

1. 报告粒度（文件 / kernel / 函数三级）与表格体量上限。
2. "冗余"是否包含未接线的调试视图与 BRP 字段（先列不删）。
3. 结构性缺漏（视图 16/17、`scgi_ms`、计数扩容、TLAS 重建、AS compaction、压缩格式）并入 9.10 还是各自另立。
4. Unity URP 版 `PatchAllocation.compute` 与 core 版的语义差异归属（9.5 还是 9.7）。
5. 审计基线场景与机位（默认场景 + 薄墙/漏光 + 室内外过渡）的最终清单。
6. 八段 §8 索引的补全方式（回填原表 vs 追加"9.10 补全版"）。

## 6. 与前后段的接口

- 与 9.9：用 `color_scope_get`/颜色面板复跑读数判定线；9.10 改画面后必复跑 9.9 判定线④⑤⑧⑩（含校色强度三档）。
- 与 9.8：P0 必须在 9.8 开工前闭环；9.8 的压测默认参数以 9.10 定稿为准；9.10 不替 9.8 出动态验收结论（9.8 是普通功能段，不是本步收尾）。
- 与 9.1/9.2：只核对被 SCGI 消费的环境 cubemap/太阳参数接驳；不评天空/IBL 模型本体。
- 与 9.4/9.5：视图扩宽、计数扩容、AS compaction、TLAS 重建等顺延项的归属由本段核销结论定。
- 施工顺序：编号追加在 9.8 之后，建议施工顺序 9.9 → 9.10 → 9.8；9.8 只是普通功能段（动态验收、压测与局限对照），不是步骤 9 的收尾，排序前置只为让压测读到修复后的画面。

## 判定线草案（段级汇总）

1. **覆盖 100%**：40 个 Unity 文件全有归属行（`AssemblyInfo.cs`/`asmdef` 标"不适用"）；每个 compute kernel 与 hlsl 导出函数都有结论；无"未审"行。
2. **三张清单齐**：缺漏/冗余/错误每条含现象、两边 `file:line` 证据、影响面、分级、处置、归属；不允许无证据条目。
3. **参数对照 ≥20 项**：两边值 + 一致性 + 不一致理由。
4. **已知项核销**：9.7 顺延 6 项逐项给"本段修/另立/不做"结论并留证据。
5. **运行时证据**：≥3 组 BRP 读数 + ≥2 张读图（默认场景与漏光案例），可复现（命令写进报告）。
6. **P0 闭环**：每项改前/改后读数或读图 + 回归结果；零新 VUID、clippy 全净、无新依赖。
7. **既有回归**：与 P0 改动相关的 `gi_*.py` 全过；无关失败项单独列出并说明"审计前已存在"。
8. **文档同步**：README/路线图段表状态、9.7 状态失真处、八段 §8 索引补全（或登记）完成。
9. **不把机制差异当缺漏**：`ObjectDispatcher`/Entity 桥/`FallbackMaterial` 等按 §0.1 口径给"机制不同，等价物在 X"，不计入缺漏。
