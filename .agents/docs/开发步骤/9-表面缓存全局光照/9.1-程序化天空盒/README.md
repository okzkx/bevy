# 9.1 程序化天空盒（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 的背景是清屏色，场景没有可见天空，也没有可被 IBL 捕获的环境光来源。本段先用解析天空函数把背景换成"天顶—地平线—地面"渐变加可见太阳圆盘，并把天空参数做成每帧可调、BRP 可写的形状，为 9.2 的天空 cubemap 捕获提供唯一真源。

**一句话说明**：用解析天空函数替换清屏色，产出可见的程序化天空与可复用的 `sky_radiance(dir)`；本段不捕获 cubemap、不投影 SH、不改任何材质的环境光。

**当前状态：✅ 收官（2026-11-23，[施工计划：程序化天空盒](施工计划：程序化天空盒.md)）。** 施工记录见下方"施工记录"，功能流程导读见[程序化天空：从参数到背景像素](程序化天空：从参数到背景像素.md)。

## 代码落点（实际）

- `shaders/common/sky.wgsl`（新）：`sky_radiance(dir, include_sun)` 与太阳圆盘/光晕函数；不声明绑定，参数来自 `FrameUniforms`。
- `shaders/scene/gbuffer/gbuffer_composite.wgsl`：背景分支从 `CLEAR_COLOR` 改为程序化天空（`sky_enabled` 关时原样回清屏色）。
- `src/render/resident/tables/frame_uniform_table.rs`：帧 UBO 208→272B（`sky_enabled`/`sky_intensity`/四行天空参数），布局断言与打包。
- `src/render/passes/sky/mod.rs`（新）：`SkySettings` 资源与默认梯度；9.2 起同目录扩捕获与 SH 投影。
- `src/remote/features/sky.rs`（新）：`sky_set` / `sky_stats` BRP 方法族。
- `src/overlay/tool_window/sky.rs`（新）：程序化天空盒面板页（用户追加要求，见 [9.1.4](9.1.4-设置资源BRP与天空面板.md)）。
- `src/driver/host/frame/scene.rs`：`SceneInput` 增加天空资源，`write_frame_ubo` 写入六字段。

## 任务面板

| 任务 | 交付（详见段施工计划） | 状态 |
|---|---|---|
| 9.1.1 天空函数 | `sky.wgsl`：垂直渐变 + 太阳圆盘/光晕；`include_sun` 参数为 9.2 捕获预留 | ✅ |
| 9.1.2 帧 UBO 扩列与宿主链 | `FRAME_UBO_SIZE` 208→272、布局断言、`FrameUniformsData.sky_*`、`write_frame_ubo` 扩参 | ✅ |
| 9.1.3 背景合成与开关 | 合成 pass 背景分支 = `sky_radiance * EXPOSURE`；`sky_enabled` 关时逐位回清屏色 | ✅ |
| 9.1.4 设置资源、BRP 与天空面板 | `SkySettings` + 默认梯度 + `sky_set`/`sky_stats` + 天空页 | ✅ |
| 9.1.5 判定线与计量 | sky 关基线截图、sky 开读图、太阳方向跟随、回归矩阵 | ✅ |

## 施工记录

- [9.1.1 天空函数](9.1.1-天空函数.md)——天空模型、`SkySettings` 字段与默认梯度两轮调优（含光晕单位坑）。
- [9.1.2 帧 UBO 扩列与宿主链](9.1.2-帧UBO扩列.md)——272B 布局、三方同步清单。
- [9.1.3 背景合成与开关](9.1.3-背景合成与开关.md)——背景分支与 reverse-Z 远平面反投影 NaN 坑。
- [9.1.4 设置资源、BRP 与天空面板](9.1.4-设置资源BRP与天空面板.md)——资源、方法族、面板页与面板验证方式。
- [9.1.5 判定线与计量](9.1.5-判定线与计量.md)——逐条判定线证据、回归读数与测试方法坑。
- [程序化天空：从参数到背景像素](程序化天空：从参数到背景像素.md)——功能流程导读（含代码改动简要解析）。

## 材料清单

- [施工计划：程序化天空盒](施工计划：程序化天空盒.md)——天空模型、UBO 布局、任务拆分、判定线、测试方案与坑（2026-11-20 立案）。
- [施工计划：天空盒、IBL 与表面缓存全局光照八段拆解](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§2 9.1 节（段级交付/判定线草案）与 §5 目录组织。
- [Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)——太阳/环境光的单位与预曝光口径；9.2 的 DFG LUT 归属。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——环境 cubemap 的捕获与太阳圆盘排除（9.2 的依据）。
