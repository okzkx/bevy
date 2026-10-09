# 3.11：egui 实体属性面板（展示与修改）

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 节尾追加段立案。本目录是 3.11 段的任务面板与证据落位。

**目的**：把 3.10 的"能查、能选"长成"能看属性、能改属性"——选中实体的属性展示（3.11.1）与属性修改（3.11.2，Transform 起步）。

**状态：🟦 立案 + 工作计划（2026-10-09），未施工。**

> **范围修订（2026-10-09 用户拍板）**：原立案的实体动态加载、实例化、删除**移出本段，并回步骤 4**（4.1 对象增量管 spawn/despawn/换柄，4.2/4.3 管资产增量与异步到货）；3.12 的实例化/despawn BRP 方法随之顺延。本段只主张"选中实体的属性可看可改"。

## 技术底座（开工先读，勿重复侦察）

- [BRP 侦察笔记 §8](../../笔记/bevy_remote：BRP远程协议与自定义方法.md)：官方数据收集口径（`bevy_dev_tools::inspection` 的组件检视、`world_summary`、label 解析）可学；**UI 表现不学**（用户实测定案：官方面板又闪又卡）。
- 3.10 既有底座：`SelectedEntity` 选中资源（overlay 内 `pub(super)`）、`DebugUiParams` 取数束（新取数走束，不撑 pass 签名——系统参数 16 上限）、overlay 加窗三步惯例（独立文件 struct + 总控开关字段 + pass 显示门）。
- 同帧 relay：Update 改值 → PostUpdate `TransformSystems::Propagate` 传播 → `collect_scene` 采集 → Last `draw_frame`——3.11.2 改值即走此链。

## 任务面板

- [ ] **3.11.1 展示属性**（属性面板）：`entity_tree_window` 选中详情区长成属性区——
  1. **组件名全清单**：`ComponentInfo::name()`（bevy_ecs/src/component/info.rs:43，返回 `DebugName`）——零反射、存在性全量（BRP 侦察笔记 §4 口径：archetype 遍历是反射无关的全量口径）；
  2. **Transform/Name 精确展示**：局部三分量 + 全局平移（现有详情区雏形扩展）；渲染语义展示保留（Mesh/材质 AssetId + 贴图槽——比官方反射展示更贴本渲染器）；
  3. **已注册反射组件的字段只读展示**：bevy_reflect 遍历字段按 `ReflectRef` 分发只读行。**前置闸门：`AppTypeRegistry` 注册覆盖面探针**——3.1.2 材质缝补过反射注册、bevy 内置组件大多自带注册，但本宿主实际覆盖面未逐一实测；探针结论落施工记录后再定反射展示范围。
  
  判定线草案：组件清单与 archetype 实际组件集一致；各展示值与 World 一致；反射覆盖面探针有结论落账；树选中→属性区同帧联动；clippy 全净、既有判定线零回退。

- [ ] **3.11.2 修改属性**（编辑控件）：Transform 三分量编辑——egui `DragValue`（widgets/drag_value.rs:55，拖拽/键入）改 translation/rotation（euler 角度显示）/scale；写值**直接改 `Transform` 组件**（不走反射）；**操作封装为可复用函数/系统**（3.12 的 BRP `set_transform` 方法调同款实现，不养两份逻辑）；egui 仲裁门（3.9 `layer_id_at`）保证编辑拖拽不带动相机轨道。
  
  判定线草案：拖 Transform 画面同帧跟随（既有同帧 relay）；改值后树/属性区显示同步；3.9 相机回归；零 VUID、WM_CLOSE exit 0。

## 边界

- **实体级增删与动态加载出段**：spawn/实例化/despawn 归步骤 4.1，新资产加载归 4.2/4.3；`despawn` 递归删 Children 子树的口径随实体操作移步步骤 4 语境。
- GPU 池/描述符槽回收归 4.1，本段一律不主张。
- **反射编辑控件不做**：官方 details 面板走 reflect 可编辑；本段只做 Transform 精确编辑，其余组件按注册覆盖面**只读**展示，可编辑范围扩大另行立项。
- 3.12 联动：BRP 读侧复用官方 inspection 方法族；`set_transform` 写方法调 3.11.2 封装的函数；实例化/despawn 方法待步骤 4。
