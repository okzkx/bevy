# 9.5 patch 池与分配回收（任务面板）

> 2026-11-24 开工、同日收官。施工依据：[施工计划：patch 池与分配回收](施工计划：patch池与分配回收.md)（2026-11-24 立案）；
> 口径推导见 [patch 池与格子系统讲解](patch池与格子系统讲解：法线口径、播种网格与级联排列.md)。
> 本目录记录"实际做了什么"；计划文档只流转状态，不回写实现细节。

**一句话**：世界空间切成相机跟随的级联格子、每格 16 个方向桶、每 cell 一条 patch，patch 池定长环形复用——屏幕 1/4 播种按 G-buffer 反推可见表面来分配或刷新 patch，回收靠滚动/心跳/整理三件；本段只建池与生命周期，不出辐照度、不接屏幕注入，开关两种状态都不改画面。

**状态：✅ 收官（2026-11-24）**——`gi_enabled = false`（默认）与 9.3 收官态 LDR 帧逐字节一致；开启后 Sponza 中庭 3320 条 patch、分配失败 0、`gi_ms ≈ 0.5～0.9 ms`（1080p 档实测）、总显存 30.67 MiB；`gi_test.py` 六条判定线 25/25 全过、进程零 VUID/VK-ERROR。

## 任务与施工记录

| 任务 | 内容 | 记录 |
|---|---|---|
| 9.5.1 | Volume 与格子系统（`gi_cell.wgsl` + CPU 镜像 + GI UBO + 单测） | [9.5.1-Volume与格子系统](9.5.1-Volume与格子系统.md) |
| 9.5.2 | 常驻表与描述符集（cell 双表 + patch 四表 + ring/计数 + GI 集 + 显存读数） | [9.5.2-常驻表与描述符集](9.5.2-常驻表与描述符集.md) |
| 9.5.3 | 屏幕播种与分配（`patch_alloc.wgsl`：1/4 网格 + 4×4 子像素 16 帧轮转 + marks 仲裁 + 池满计数） | [9.5.3-屏幕播种与分配](9.5.3-屏幕播种与分配.md) |
| 9.5.4 | 回收三件（`scrolling` / `eviction` / `defrag`） | [9.5.4-回收三件](9.5.4-回收三件.md) |
| 9.5.5 | 录制编排与开关（④.5 段、`GiFrameDraw`、`GiSettings`、时间戳 T5、后链拆分） | [9.5.5-录制编排与开关](9.5.5-录制编排与开关.md) |
| 9.5.6 | patch 调试视图两形态与 BRP 四口（`gi_set`/`gi_stats`/`gi_dump`/`gi_census`） | [9.5.6-patch调试视图与BRP四口](9.5.6-patch调试视图与BRP四口.md) |
| 9.5.7 | 判定线与计量（`gi_test.py`、`gi_ms`、显存组） | [9.5.7-判定线与计量](9.5.7-判定线与计量.md) |

功能流程走读：[patch 池与分配回收流程](patch池与分配回收流程.md)（从相机位置到最终像素的每次数据流）。

## 判定线结果（`gi_test.py`，2026-11-24）

| # | 判定 | 结果 | 证据 |
|---|---|---|---|
| ① | 分布与覆盖吻合 | ✅ | 覆盖图绿 97.1% / 红 0.0% / 蓝 2.9%；位置偏离红 1.0%；桶着色 15 色；cell 归属 513 色；`gi_census` 16/16 双向一致、在体素内、法线在桶内；方片可见 |
| ② | 显存恒定 | ✅ | 连跑 ≥120 帧表字节 30,671,536B 不变；水位 3320 ≤ 65536 |
| ③ | 跟随无残留 | ✅ | 独享墙体进视野 3320→3461；移出视野 7s（>240 帧淘汰窗）回到 3320；census 无悬空 |
| ④ | 稳态平衡 | ✅ | 静止 2s：failed 0、active 3320→3320、水位 3590 不涨；defrag 后 census 全过；`gi_ms` 0.54～0.88 |
| ⑤ | 大世界档可游历 | ✅ | 关态改 Size 512 / 6 级联 / 池 2^18 → 开启后 4253 条、failed 0、cell 表 25.2 MiB；开态改规格被拒 |
| ⑥ | 不破底座 | ✅ | GI 开（view=0）→关：LDR 帧逐字节一致；关态 `gi_stats` enabled:false / `gi_dump` 被拒 / `gi_census` 回 enabled:false；零 VUID |

代码验证线：`cargo test -p ash_renderer --lib` 113 项全过（含布局契约与新增单测）、`cargo clippy -p ash_renderer` 零告警；五脚本回归（`hiz_test` / `scene_lod` / `scene_stream` / `scene_fine` / `scene_sponza`）结果见 [9.5.7 记录](9.5.7-判定线与计量.md)。

## 关键裁决与偏差（相对计划）

1. **G-buffer 采样集走"扩阶段位"**：`GBUFFER_SAMPLED_SET_DESC` 七条 binding 与 `IBL_SET_DESC` 的 b3/b4 扩到含 `COMPUTE`——一份布局一份集，播种 compute 直接绑现有采样集；零 VUID，契约测试全过（计划前置闸门 1 的两案取改动小的）。
2. **`PatchStatistics` 48B**（计划 §0.2 写 32B，但两份 vec4 后还有 4 个 u32）：patch 表 13.25 MiB，总表 30.67 MiB（计划估算 ≈28.5 MiB）。
3. **cell marks 用帧号纪元标签**（`frameIdx | 1`）而不是 1/0：省掉每帧清 8 MiB marks 的 dispatch；标签用 `atomicExchange` 比较旧值判断同帧唯一分配权。
4. **defrag 不用 workgroup 前缀和**：naga 30 给 `<workgroup>` 聚合发 Offset/ArrayStride，被 spirv-val 按 VUID-StandaloneSpirv-None-10684 拒（构建门只补描述符索引，不为它开路）。改为**原子取号压缩**：每波 512 条（256 线程 × 2 条）、2 波/帧（计划 §4 的"2×512 线程"），目标偏移恒 ≤ 源偏移；源记录先读进私有存储、`workgroupBarrier + storageBarrier` 之后才落笔。全池走完 ≈64 帧。
5. **视图编号 0 = 关**（计划 §9.5.6 的显式双门 `gi_enabled && view > 0`）：1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 / 6 池水位 / 7 位置偏离 / **8 cell 归属**（计划 §0.7 把 cell 记作 0，与门冲突，这里顺延到 8）。
6. **位置偏离看"离面"不看"离点"**：视图 7 = patch 位置到当前表面切平面的垂直距离 / 体素。一个 cell 覆盖整个体素，当前像素与存的位置切向可差一整格，欧氏距离天然超阈值；离面距离才是"贴不贴表面"。
7. **视图画进 LDR 后链目标**（SMAA edge/blend 之后、neighborhood 之前）：`gi_dump` 复用 `frame_dump` 的 LDR→PPM 通道，零新增读回；后链录制拆成 `record_to_ldr` / `record_neighborhood` 两步，调用边界变化、命令序列不变。
8. **GI 集布局归 `GiSetLayout` 独有**：首次实现把布局放在表里，改规格重建表时销毁了布局，已建管线的 pipeline layout 引用悬空（`VUID-vkCmdBindPipeline-pipeline-parameter`）；改为布局进程级、表只换 buffer/pool/sets。
9. **表在 init 急建**（计划允许懒建）：30.67 MiB 常驻、关态零每帧成本；BRP 读数与运行期开局都不需要重建。

## 顺延与遗留

- 计划 §5 的待决项：法线来源默认平面法线（A/B 开关保留）、1/4 播种固定、体素 warping 不做、defrag 2 波/帧、池 65536、cell 表 HOST_VISIBLE（初值写 ~几十毫秒未单独计）、视图默认覆盖图 + 方片交叉验证。
- **motion vectors 本体未做**（计划 §0.4 允许的降级）：本段种子是 IBL 常量，无低分辨率辐照度可重投影；`prev_view_proj` 字段与"另一帧槽模型环 = 上一帧变换"的结论已留好，9.6 接 MV 时只补附件 2 与 VS 输出。
- **`gi_ms` 实测比计划估值高**（0.5～0.9 ms vs 0.1～0.3 ms）：defrag 两波 + 淘汰 1024 组 + 播种 129,600 线程与 barriers 占主；9.8 压测时再按 `defrag_waves` 档位与 1/4→1/2 播种做取舍。
- 9.6 的 bounce 降 rank 分支与 `bounce_enabled` 字段已留（eviction 里实现），本段恒 0；9.7 的查表入口 = `gi_cell.wgsl` 的同一组函数，法线来源必须与播种同源。

## 材料清单

- 计划：[施工计划：patch 池与分配回收](施工计划：patch池与分配回收.md)（2026-11-24 立案）。
- 口径：[patch 池与格子系统讲解](patch池与格子系统讲解：法线口径、播种网格与级联排列.md)（2026-11-24）。
- 记录：上表七篇 + [功能流程导读](patch池与分配回收流程.md)。
- 工具：`tools/gi_test.py`（六条判定线一键驱动）、`tools/brp.py` 的 `gi-set` / `gi-stats` / `gi-dump` / `gi-census` 四个子命令。
- 代码落点：`shaders/common/gi_cell.wgsl`、`shaders/scene/gi/{patch_alloc,scrolling,eviction,defrag,patch_view}.wgsl`、`src/render/passes/gi/{mod,volume,allocation,view}.rs`、`src/render/resident/tables/surface_cache.rs`、`src/driver/host/frame/gi.rs`、`src/remote/features/gi.rs`。
