# 9.2 天空盒 IBL（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：9.1 只让天空"看得见"，材质仍然吃常量环境项。本段把天空捕获成环境 cubemap，按标准 IBL 拆成两半：漫反射用 SH-L2 辐照度，高光用 GGX 预滤波 cubemap + BRDF/DFG LUT；然后在不透明光照累积与透明前向里替换常量环境项。这是步骤 9 的环境光基线，后续 9.3 的 GlobalProbe 与 9.7 的 SCGI 都在这条基线上做增量。

**一句话说明**：把 9.1 的程序化天空捕获为环境 cubemap，投影 SH-L2 漫反射并预滤波高光，替换不透明与透明的常量环境项；本段不产生世界空间缓存、射线或 SCGI。

**当前状态：📝 施工计划已立案，未开工（2026-11-20，[施工计划：天空盒IBL](施工计划：天空盒IBL.md)）**。

## 代码落点（提案）

- `shaders/common/ibl.wgsl`（新）：SH-L2 求值、方向/立方体面映射、split-sum 求值；不声明绑定。
- `shaders/scene/ibl/`（新）：`sky_capture.wgsl`（天空 → cubemap）、`sh_project.wgsl`（cubemap → SH-L2）、`prefilter.wgsl`（GGX 预滤波 mip 链）、`brdf_lut.wgsl`（DFG LUT）。
- `shaders/scene/gbuffer/gbuffer_composite.wgsl`：环境项替换为 IBL 漫反射 + 高光；新增两档调试视图。
- `shaders/scene/transparent/transparent_forward.wgsl`：透明层同样接 IBL（新的环境基线），但不接 SCGI。
- `src/render/passes/ibl/`（新）：cubemap 资源、捕获/投影/预滤波 pass 录制、脏标记与节流。
- `src/render/passes/lighting/composite_pipeline.rs`、`src/render/passes/transparent/transparent_pipeline.rs`：各加一组 IBL 描述符集（group 4）。
- `src/render/resident/tables/frame_uniform_table.rs`：帧 UBO 272→288B（`ibl_enabled`/`ibl_intensity`/`ibl_mode`）。
- `src/remote/features/ibl.rs`（新）：`ibl_set` / `ibl_stats` BRP 方法族。

## 任务面板（随段施工计划 §1 同步）

| 任务 | 交付（详见段施工计划） | 状态 |
|---|---|---|
| 9.2.1 环境 cubemap 捕获 | 256²×6 RGBA16F、cube 兼容、mip 链；从 `sky_radiance(dir, false)` 捕获；脏标记 + 节流；cube/face/mip 视图 | ⬜ |
| 9.2.2 SH-L2 投影 | cubemap → 9 系数 × RGB（折叠余弦卷积）；storage buffer；能量自检 | ⬜ |
| 9.2.3 漫反射注入 | 不透明/透明环境项替换为 `diffuseColor/π · E(n) · ibl_intensity`；`ibl_enabled` 关时逐位回退 | ⬜ |
| 9.2.4 高光预滤波与 DFG LUT | GGX 预滤波 mip 链 + 128² DFG LUT（或解析近似）；粗糙度→mip | ⬜ |
| 9.2.5 高光注入 | split-sum：`prefiltered(R) · (F0·A + B)`；太阳高光仍走直接光 | ⬜ |
| 9.2.6 描述符/管线/调试/BRP | 两组 IBL 描述符集；`ibl_set`/`ibl_stats`；mode 9/10 调试视图 | ⬜ |
| 9.2.7 判定线与计量 | IBL 关基线、均匀天空自检、金属/透明/室内对照、成本读数、回归矩阵 | ⬜ |

## 材料清单

- [施工计划：天空盒IBL](施工计划：天空盒IBL.md)——数据链、SH-L2/预滤波口径、UBO 与描述符布局、任务拆分、判定线、测试方案与坑（2026-11-20 立案）。
- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§2 9.2 节（段级交付/判定线草案）与 §5 目录组织。
- [Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)——split-sum、DFG LUT、`energyCompensation` 与预曝光口径；8.2 已把 DFG LUT 留给 IBL。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——环境 cubemap 的捕获、太阳圆盘排除、`GlobalProbe` 的更新条件（9.3 的源码依据）。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——一个探针=一个点的全方向展开；L2 与 16 桶的分工。
