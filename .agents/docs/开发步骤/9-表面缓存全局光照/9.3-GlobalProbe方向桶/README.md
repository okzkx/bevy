# 9.3 GlobalProbe 方向桶（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：9.2 的漫反射是单个 SH-L2（空间不变、方向连续）。本段从同一张天空 cubemap 再生成一份 **16 方向桶 × RGBL1** 的全局探针：4×4 八面体分桶、每桶半球采样 32 方向、投影成 L1 并余弦卷积。它有两个用途：一是与 9.4 之后的 patch 数据格式同构，给 9.7 的屏幕 gather 提供体积外兜底；二是作为 L2 之外的另一种方向类别 LUT，用 A/B 决定默认。

**一句话说明**：从 9.2 的天空 cubemap 生成 16 桶 × RGBL1 的 GlobalProbe，按法线选桶求值，并用 A/B 对照 L2；本段不产生空间缓存、不发射射线、不替代 L2 作为默认（除非数据支持）。

**当前状态：📝 施工计划已立案，未开工（2026-11-20，[施工计划：GlobalProbe方向桶](施工计划：GlobalProbe方向桶.md)）**。

## 代码落点（提案）

- `shaders/common/octahedral.wgsl`（新）：八面体球↔方映射、方向→桶索引、桶中心方向；9.5/9.7 的 cell/patch 桶位共用同一份。
- `shaders/scene/ibl/global_probe.wgsl`（新）：16 桶 × 32 样本的 L1 投影与余弦卷积；写 16×RGBL1 buffer。
- `shaders/common/ibl.wgsl`：增加 `probe16_eval(n)` 与 `ibl_mode` 分支。
- `src/render/passes/ibl/`：GlobalProbe buffer、脏标记复用 9.2 的捕获节流、pass 录制。
- `src/remote/features/ibl.rs`：`ibl_set` 扩 `diffuse_mode`，`ibl_stats` 回显 16 桶摘要。
- `src/render/frame/render_mode.rs`：可选桶可视化调试档（若加，序号接在 9.2 的 mode 9/10 之后）。

## 任务面板（随段施工计划 §1 同步）

| 任务 | 交付（详见段施工计划） | 状态 |
|---|---|---|
| 9.3.1 八面体公共片段 | `octahedral.wgsl`：映射、`direction_to_bucket_index`、`bucket_index_to_direction`；与 Unity `Common.hlsl:161-182` 同式 | ⬜ |
| 9.3.2 16 桶 L1 投影 | `global_probe.wgsl`：16 组、每组 32 样本、组内归约、`A0/A1` 卷积；16×48B buffer | ⬜ |
| 9.3.3 查询与模式切换 | `ibl_mode` 0 = SH-L2、1 = GlobalProbe；composite/transparent 的漫反射分支 | ⬜ |
| 9.3.4 BRP 与调试视图 | `ibl_set {"diffuse_mode":"sh_l2"|"probe16"}`；`ibl_stats` 16 桶摘要；桶可视化（可选） | ⬜ |
| 9.3.5 判定线与 A/B | 桶方向正确性、均匀天空自检、L2 vs 16 桶读图、接缝/收益记录、回归矩阵 | ⬜ |

## 材料清单

- [施工计划：GlobalProbe方向桶](施工计划：GlobalProbe方向桶.md)——算法、映射约定、A/B 口径、任务拆分、判定线、测试方案与坑（2026-11-20 立案）。
- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§2 9.3 节与 §5 目录组织。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——GlobalProbe 16 桶与 Cell 16 槽的区别、16 桶 vs L2 的对照。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——`GlobalProbe.compute` / `GlobalProbe.hlsl` 的源码级依据。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——`DistanceFallback` 的定位与质量档。
