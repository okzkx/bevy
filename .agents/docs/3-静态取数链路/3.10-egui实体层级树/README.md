# 3.10：egui 实体层级树（检视）

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 节尾追加段立案。本目录是 3.10 段的任务面板与证据落位。

**目的**：给 3.7 的调试 GUI 补"看内容"——World 实体按 `Children`/`ChildOf` 关系递归展示，点选落账为 3.11 实体编辑的目标来源。

**状态：✅ 已收官（2026-10-09，3.10.1 一个任务）。**结构按用户当日拍板调整：层级树独立 rs 文件 struct（非立案时设想的 debug_window.rs 新页），并新增"调试窗口总控"窗口统一开关各调试 UI。判定线全过（树层级对齐 3.1.4 父链判定、运行期增删当帧反映、详情数值与 World 一致、帧率无可感影响、既有判定线零回退），全程零 VUID，WM_CLOSE exit 0。

## 任务面板

- [x] **3.10.1 实体层级树窗口**：`overlay/entity_tree_window.rs`（`SelectedEntity` 点选资源 + `EntityRow` 查询 + `EntityTreeData` SystemParam 束 + `EntityTreeWindow` struct）；`overlay/debug_hub_window.rs`（`DebugWindowsOpen` 显隐资源 + 总控面板）；`ui.rs` 接线（init 资源 + `DebugUiParams` 参数束 + 三窗口显隐门）；`debug_window.rs` 加 `open` 字段（[×] 与总控写同字段）。实录见[施工记录](3.10.1-实体层级树施工记录：独立窗口struct、总控窗口与相关实体过滤.md)。

## 判定线（对照立案独立验收）

| 判定线 | 结果 |
|---|---|
| 树结构与 World 实际层级一致（FlightHelmet 父链对齐 3.1.4 采集判定线） | ✅ 树形 `WorldAssetRoot ▼ Scene0 ▼ 六 node ▼ primitive [mesh][mat]`，primitive 实体号 #376~#381 与采集快照互证 |
| 运行期增删实体当帧/次帧反映 | ✅ 临时探针 2.5s 周期 spawn/despawn，树行与"相关"计数当帧反映（探针验证后已删除） |
| 显示数值与 World 一致 | ✅ 详情区 Transform 与场景 spawn 恒等矩阵一致；Mesh/材质 AssetId 与 collect 快照同实体号 |
| 树展开对帧率无可感影响 | ✅ 树满开下 fps 54~60（Reactive 节流），拖拽输入期 111 |
| 既有判定线零回退 | ✅ 3.5 三模式面板在、3.7 窗口可开关、3.9 相机拖拽换面；两份全量日志零 VUID（含拆 Device）；WM_CLOSE exit 0 |

## 边界

- 本段只立"能查、能选"；3.11 的 Transform 编辑以 `SelectedEntity` 为目标来源（编辑操作封装为可复用函数，供 3.12 BRP 方法调同款实现）。
- GPU 池/描述符槽回收不在此主张（归步骤 4.1 对象增量）。
