# 9.1 间接光注入与 SH 查询（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 的光照累积 pass 里，间接光只是一个常量环境项。本段把它换成"可替换的辐照度数据源"——先用单个全局 L1 SH 探针占位，把注入接口、SH 数学、GI 开关与 A/B 计量钉死；后续 9.2～9.5 只换数据来源（求交器 → patch 池 → irradiance 更新 → 屏幕 gather），注入端与 SH 求值不再动。

**一句话说明**：用全局 L1 SH 探针把间接漫反射接进光照累积 pass——`E(n) = l0 + l1·n`、`indirect = diffuseColor/π · E`；GI 关闭时保留步骤 8 原文，零射线、零新 pass、零世界空间缓存。

**当前状态：📝 施工计划已立案，未开工（2026-11-20，[施工计划：间接光注入与SH查询](施工计划：间接光注入与SH查询.md)）**。

## 代码落点（提案）

- `shaders/common/gi.wgsl`（新）：SH 公共片段——`GiProbeSh`、投影/累加、余弦卷积、折叠基函数、辐照度求值。
- `shaders/scene/gbuffer/gbuffer_composite.wgsl`：间接项替换环境项；新增两档调试视图。
- `src/render/resident/tables/frame_uniform_table.rs`：帧 UBO 208→272B（`gi_enabled`/`gi_intensity`/`gi_probe`），布局断言与打包。
- `src/render/passes/gi/mod.rs`（新）：`GiSettings` 与默认探针解析；9.3 起同目录扩 allocation/estimation 等 pass 录制。
- `src/remote/features/gi.rs`（新）：`gi_set` / `gi_stats` BRP 方法族。

## 任务面板（随段施工计划 §1 同步）

| 任务 | 交付（详见段施工计划） | 状态 |
|---|---|---|
| 9.1.1 SH 数学公共片段 | `gi.wgsl`：`GiProbeSh`、Y0/Y1 + A0/A1、投影/卷积/折叠/求值；不声明绑定 | ⬜ |
| 9.1.2 帧 UBO 扩列与宿主链 | `FRAME_UBO_SIZE` 208→272、布局断言、`FrameUniformsData.gi_*`、`write_frame_ubo` 扩参 | ⬜ |
| 9.1.3 注入与调试视图 | 合成 pass 间接项 = `diffuseColor/π·E`（GI 关保留原文）；`FRAME_MODE_GI` / `FRAME_MODE_GI_PROBE` | ⬜ |
| 9.1.4 设置资源与 BRP | `GiSettings`（默认跟随环境光）+ `init_resource` + `gi_set`/`gi_stats` | ⬜ |
| 9.1.5 判定线与计量 | GI 关基线截图、GI 开 A/B 读图、`gpu_lighting_ms` 成组、回归矩阵 | ⬜ |

## 材料清单

- [施工计划：间接光注入与SH查询](施工计划：间接光注入与SH查询.md)——段级任务拆解、SH 口径、UBO 布局、判定线、测试方案与坑（2026-11-20 立案）。
- [施工计划：表面缓存全局光照六段拆解](../施工计划：表面缓存全局光照六段拆解.md)——§2 9.1 节（段级交付/判定线草案）与 §5 目录组织。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——SH 投影/余弦卷积、L1 压缩与预曝光的源码级依据。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——全局探针与 `DistanceFallback` 的定位。
