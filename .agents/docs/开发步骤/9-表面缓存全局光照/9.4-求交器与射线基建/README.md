# 9.4 求交器与射线基建（M8）

对应[步骤 9 README](../README.md)与[路线图步骤 9 节](../../学习目标实现步骤.md)。

**目的**：把"射线怎么打、打到什么、命中后怎么取属性"做成与 GI 无关的基建。硬件 inline
ray query 主后端（本机 RTX 2060 已核实）；BLAS 挂 6.x 驻留资产（每 LOD 组只给最粗已驻留
变体建一份）；TLAS 每帧从在场环全量重建；命中顶点/法线/UV 从既有 `MeshPool` 重建。本段
**不改画面**：RT 开只多基建命令，RT 关与 9.3 收官态逐位一致。

**一句话说明**：CPU 只建 BLAS、写地址表、录 dispatch；射线在 compute 线程里生灭，命中只
回三角形编号，属性回网格池取。

**当前状态：✅ 收官（2026-11-24）。** 任务 9.4.1～9.4.7 全部落地；判定线 15 项
（`tools/rt_test.py`）+ 五脚本回归（hiz_test / scene_lod / scene_stream / scene_fine /
scene_sponza）全过；库测试 102 项、clippy 全净、`cargo fmt --check` 全绿、零 VUID。功能流程
走读见[求交器流程：从驻留事件到射线命中重建](求交器流程：从驻留事件到射线命中重建.md)。

## 代码落点

- `src/render/core/device/rt.rs`（新）：`RtCaps`/`RtSupport` 能力判定纯函数 + 单测。
- `src/render/core/device/context.rs`：三项 feature 查询与启用、三个扩展、加速结构属性、
  `buffer_address()`。
- `src/render/core/device/memory/{role,buffer}.rs`：`RtPool` / `AccelerationStructureScratch` /
  `AccelerationStructureStorage` / `TlasInstances`；BDA 分配 flag 从 usage 推导。
- `src/render/core/binding/{set_layout,descriptor_writer}.rs`：`ACCELERATION_STRUCTURE_KHR`
  描述符种类与写入（逐条提交 + `descriptor_count(1)`）。
- `src/render/rt/{mod,blas,tlas}.rs`（新）：`RayScene` 宿主入口、BLAS 注册表（锚档排建/
  合并录制/两级停尸房）、每帧 TLAS（容量/构建/前后屏障）。
- `src/render/passes/rt/mod.rs`（新）：RT 描述符集 b0–b4、RGBA16F 调试图、统计缓冲、
  `tlas_build`/`ray_debug` 两条管线、`RtFrameDraw` 录制束、调试图读回。
- `src/render/resident/assets/pool.rs`：`MeshPool::new` 收 RT 开关；`vertex/index_buffer_info`、
  `vertex/index_address`。
- `src/render/resident/tables/instance_table.rs`：set2 b10 地址表 / b11 TLAS 实例数组（RT 关
  时 PARTIALLY_BOUND 不写）。
- `src/driver/upload.rs`、`src/driver/host/drawlist.rs`：BLAS 排建重扫、退场/换内容排销毁、
  地址表增量与整表重写、`blas_asset_id`（最粗已驻留变体）。
- `src/driver/host/frame.rs`、`frame/{input,rt,resize_gate,stopwatch}.rs`：帧侧 RT 编排
  （容量→换绑→地址表→统计清零→产物束）、调试图随 resize 重建、GPU 计时 T9/T10。
- `src/render/frame/{record,render_features}.rs`：② 段最前插 RT 段（BLAS 待建 → TLAS 组装/
  构建 → 调试图）。
- `src/render/stats/perf.rs`：`TimestampPoint::RtTlas/RtRays`、`fetch_rt`、帧样本两字段。
- `src/remote/features/rt.rs`（新）+ `remote/mod.rs`：`rt_set` / `rt_stats` / `rt_dump` /
  `rt_census`。
- `src/render/layout_registry.rs`：`RT_SET_DESC` 与两条 RT 管线登记。
- `ash_renderer/build.rs`：校验 caps 加 `RAY_QUERY`、ABI 反射补 `AccelerationStructure`、
  `TlasInstance` 进跨端布局门。
- `shaders/common/raytracing.wgsl`（新）、`shaders/scene/rt/{tlas_build,ray_debug}.wgsl`（新）。
- `tools/rt_test.py`（新）：判定线一键驱动（15 项）。

## 任务面板

| 任务 | 交付 | 状态 |
|---|---|---|
| 9.4.1 设备能力与 feature 门 | `RtSupport` 纯函数 + 单测；BDA/AS/RQ feature 与三扩展条件启用；`RtSettings` | ✅ [记录](9.4.1-设备能力与feature门：RtSupport判定、扩展启用与RT开关.md) |
| 9.4.2 内存与描述符基建 | 四个 RT buffer 角色、BDA 分配 flag、AS 描述符种类/写入、`MeshPool` 地址口、set2 b10/b11 | ✅ [记录](9.4.2-内存与描述符基建：RT缓冲角色、BDA分配与加速结构描述符.md) |
| 9.4.3 BLAS 生命周期 | `BlasRegistry`（锚档排建/替换重建/退场销毁）；地址表增量 + generation 整表重写 | ✅ [记录](9.4.3-BLAS生命周期：锚档排建、地址表与两级停尸房.md) |
| 9.4.4 命中取数与射线抽象 | `raytracing.wgsl`（`trace_scene`/命中重建）；RT 集；`RayScene`；build.rs 两处接线 | ✅ [记录](9.4.4-命中几何取数与射线抽象：raytracing片段与RT描述符集.md) |
| 9.4.5 TLAS 每帧构建 | `tlas_build.wgsl`；`TlasFrame` 帧槽分家；② 段插入；空场景不打射线 | ✅ [记录](9.4.5-TLAS每帧构建：实例数组、构建命令与帧槽分家.md) |
| 9.4.6 调试视图与 BRP | `ray_debug.wgsl` 三子视图；`rt_set/rt_stats/rt_dump/rt_census`；T9/T10 计时 | ✅ [记录](9.4.6-调试视图与BRP：调试图、统计与PPM取证.md) |
| 9.4.7 判定线与计量 | `tools/rt_test.py` 15 项 + 五脚本回归 + 读图取证；计量口径 | ✅ [记录](9.4.7-判定线与计量：rt_test十五项与五脚本回归.md) |

## 材料清单

- [施工计划：求交器与射线基建](施工计划：求交器与射线基建.md)——裁决、任务拆解、判定线草案
  （2026-11-24 立案；同日施工收官）。
- [求交器流程：从驻留事件到射线命中重建](求交器流程：从驻留事件到射线命中重建.md)——全流程
  走读与代码改动解析（收官篇）。
- [9.4.1](9.4.1-设备能力与feature门：RtSupport判定、扩展启用与RT开关.md) /
  [9.4.2](9.4.2-内存与描述符基建：RT缓冲角色、BDA分配与加速结构描述符.md) /
  [9.4.3](9.4.3-BLAS生命周期：锚档排建、地址表与两级停尸房.md) /
  [9.4.4](9.4.4-命中几何取数与射线抽象：raytracing片段与RT描述符集.md) /
  [9.4.5](9.4.5-TLAS每帧构建：实例数组、构建命令与帧槽分家.md) /
  [9.4.6](9.4.6-调试视图与BRP：调试图、统计与PPM取证.md) /
  [9.4.7](9.4.7-判定线与计量：rt_test十五项与五脚本回归.md) 施工记录。
- 立案期知识文档：[加速结构讲解](加速结构讲解：BLAS、TLAS 与两层 BVH.md)、
  [执行讲解](求交器执行讲解：dispatch、队列、RT Core 与 API 选型.md)、
  [数据讲解](求交器数据讲解：几何复用、射线生成与射线量预算.md)、
  [模块讲解](求交器模块讲解：接口两面、落点与后端可替换.md)。
