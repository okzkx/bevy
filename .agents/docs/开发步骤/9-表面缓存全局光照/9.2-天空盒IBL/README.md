# 9.2 天空盒 IBL（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：9.1 只让天空"看得见"，材质仍然吃常量环境项。本段把 9.1 的物理天空捕获成环境 cubemap，按标准 IBL 拆成两半：漫反射用 SH-L2 辐照度，高光用 GGX 预滤波 cubemap + BRDF/DFG LUT；然后在不透明光照累积与透明前向里替换常量环境项。这是步骤 9 的环境光基线，后续 9.3 的 GlobalProbe 与 9.7 的 SCGI 都在这条基线上做增量。

**一句话说明**：把 9.1 的物理天空捕获为环境 cubemap，投影 SH-L2 漫反射并预滤波高光，替换不透明与透明的常量环境项；本段不产生世界空间缓存、射线或 SCGI。

**当前状态：✅ 收官（2026-11-23）。九任务 9.2.0～9.2.8 全部落地，判定线 12 条全过；9.2.8 为收官后现场复检补修（高光火花 / DFG 积分器 / 天空地平线噪点 / 天光开关联动）；施工记录见下表，功能流程走读见[天空盒IBL流程](天空盒IBL流程.md)。**

## 代码落点（提案）

- `shaders/common/ibl.wgsl`（新）：SH-L2 求值、方向/立方体面映射、split-sum 求值；不声明绑定。
- `shaders/scene/ibl/`（新）：`sky_capture.wgsl`（天空 → cubemap）、`sh_project.wgsl`（cubemap → SH-L2 + 两条自检）、`prefilter.wgsl`（GGX 预滤波 mip 1..8）、`brdf_lut.wgsl`（DFG LUT）。
- `shaders/common/sky.wgsl`：加 `sky_uniform` 常量分支（9.2.0，测试开关，默认关）。
- `shaders/scene/gbuffer/gbuffer_composite.wgsl`：环境项替换为 IBL 漫反射 + 高光；新增两档调试视图。
- `shaders/scene/transparent/transparent_forward.wgsl`：透明层同样接 IBL（新的环境基线），但不接 SCGI。
- `src/render/passes/ibl/`（新）：cubemap 资源、捕获/投影/预滤波/DFG pass 录制、脏标记与节流。
- `src/render/core/record/barrier.rs`：`ImageBarrier` 加子资源范围字段（cube 需要 9 mip × 6 层）。
- `src/render/passes/lighting/composite_pipeline.rs`、`src/render/passes/transparent/transparent_pipeline.rs`：各加一组 IBL 描述符集（group 4）。
- `src/render/frame/record.rs`（② 段插捕获、⑤ 段绑 IBL set）与 `src/render/frame/render_features.rs`（新 IBL 束字段）；`src/driver/host/init.rs`（`SetLayouts` 登记 IBL 布局）。
- `src/render/resident/tables/frame_uniform_table.rs`：帧 UBO 304→336B（`ibl_enabled`/`ibl_mode`/`ibl_intensity` + `sky_uniform`）。
- `src/remote/features/ibl.rs`（新）：`ibl_set` / `ibl_stats` BRP 方法族；`src/remote/features/sky.rs`：`sky_uniform`（9.2.0）。

## 任务面板（随段施工计划 §1 同步）

| 任务 | 交付（详见段施工计划） | 状态 |
|---|---|---|
| 9.2.0 常量天空开关与 UBO 扩列 | 帧 UBO 304→336；`sky_radiance` 常量分支；`sky_uniform` BRP；布局/打包/头注四处同步 | ✅ [记录](9.2.0-常量天空开关与帧UBO扩列.md) |
| 9.2.1 环境 cubemap 捕获 | 256²×6 RGBA16F、cube 兼容、9 级 mip；从 `sky_radiance(dir, false)` 捕获（只排圆盘，米氏峰保留）；`sky_hash`（含 100 m 量化海拔）+ 0.1s 节流 + `ibl_enabled` 门控；cube/2D-array/每 mip 视图；全子资源屏障 | ✅ [记录](9.2.1-环境cubemap捕获.md) |
| 9.2.2 SH-L2 投影 | cubemap → 9 系数 × RGB（折叠余弦卷积）；storage buffer；两条自检（l=0 归一化、`E_sh` 对暴力积分） | ✅ [记录](9.2.2-SH-L2投影与自检.md) |
| 9.2.3 漫反射注入 | 不透明/透明环境项替换为 `diffuseColor/π · E(n) · ibl_intensity`；IBL 关时逐位回退；IBL set 建集并绑进两条管线 | ✅ [记录](9.2.3-漫反射注入与IBL描述符集.md) |
| 9.2.4 高光预滤波与 DFG LUT | GGX 预滤波 mip 1..8（mip 0 保留锐利源）+ 128² DFG LUT（或解析近似）；粗糙度→mip | ✅ [记录](9.2.4-高光预滤波与DFG-LUT.md) |
| 9.2.5 高光注入 | split-sum：`prefiltered(R) · (F0·A + B)`；太阳高光仍走直接光 | ✅ [记录](9.2.5-高光注入.md) |
| 9.2.6 调试/BRP/管线收口 | `ibl_set`/`ibl_stats`；mode 9/10 调试视图；`RenderMode::ALL` 9→11 | ✅ [记录](9.2.6-调试视图与BRP.md) |
| 9.2.7 判定线与计量 | IBL 关基线、`sky_uniform` 均匀天空自检、太阳排除负例、金属/透明/室内对照、捕获成本读数、回归矩阵 | ✅ [记录](9.2.7-判定线与计量.md) |
| 9.2.8 现场复检补修 | DFG 积分器 G_Vis 修正（火花主因）、IBL 视向侧法线 + specular AA、`sky_enabled` 联动环境光与捕获、天空海拔 1 m 兜底 + 球求交稳定式、`ibl_census` 加 `row` | ✅ [记录](9.2.8-高光火花、天空地平线噪点与天光开关联动.md) |

## 材料清单

- [施工计划：天空盒IBL](施工计划：天空盒IBL.md)——数据链、SH-L2/预滤波口径、UBO 与描述符布局、任务拆分、判定线、测试方案与坑（2026-11-20 立案；2026-11-23 按物理天空同步修订）。
- [9.2.8 现场复检补修](9.2.8-高光火花、天空地平线噪点与天光开关联动.md)——DFG 积分器缺陷的隔离实验与修法、天空地平线噪点的数值退化根因、天光开关联动语义与跨构建逐位证据。
- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§2 9.2 节（段级交付/判定线草案）与 §5 目录组织。
- [物理天空追加段](../9.1-程序化天空盒/施工计划：物理天空（9.1追加）.md)与[9.1.6 参数面](../9.1-程序化天空盒/9.1.6-物理天空模型与参数面.md)——捕获源的现行形态、相机海拔假设、太阳盘/米氏峰语义与 `SkySettings` 字段。
- [9.1.7 判定线与计量（物理天空）](../9.1-程序化天空盒/9.1.7-判定线与计量（物理天空）.md)——天空成本读数与回归口径，9.2 的 A/B 基线。
- [Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)——split-sum、DFG LUT、`energyCompensation` 与预曝光口径；8.2 已把 DFG LUT 留给 IBL。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——环境 cubemap 的捕获、太阳圆盘排除、`GlobalProbe` 的更新条件（9.3 的源码依据）。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——一个探针=一个点的全方向展开；L2 与 16 桶的分工。
