# egui 树重建裁剪流程：从脏信号到视口行的各级账面

面向下一个读代码的人：7.7 把实体树窗口从"每帧全量重建全部实体行"改成"拍平缓存 + 只构建可见行"。这篇按"一次实体 spawn 如何走到屏幕上的一行"走读全链，文末附代码改动简要解析。收益主战场是 **pass 求值与逐帧分配**——tessellate 半边在 7.7 之前就已有 clip rect 裁剪，不受益。

## 1. 各级账面全景

| 层 | 账面 | 谁写 | 谁读 |
|---|---|---|---|
| 脏信号 | `EntityTreeState.dirty` | 三观测器（Insert/Remove/Despawn 裸事件）+ UI 展开/收起 | 维护系统 |
| 展开态 | `EntityTreeState.expanded: HashMap<Entity, bool>` | UI 点击写；重建时首见落默认账（深度<2 展开）、死实体清扫 | 重建走访 |
| 拍平行 | `EntityTreeState.rows: Vec<TreeRow>`（等高行，含 depth/label/has_children/expanded） | 维护系统（脏才重建，全量走访 relevant 表） | egui pass（只读切片） |
| 计数 | `relevant`/`total` | 同上 | 树窗首行"相关 X / 全部 Y" |
| 共享语义 | `is_relevant`/`node_label`/`EntityRow` | ——（单实现，BRP `scene_tree` 同源） | 重建与 CLI 树 |

## 2. 逐时刻走读

**spawn / despawn / 换柄**：命令 flush 派发生命周期事件 → `mark_tree_dirty_*` 观测器（`On<InsertEvent>` 等**无组件模式**裸事件，任一组件变动都算候选树形态变化）置 `dirty = true`。稳态零观测器触发、零成本。

**下帧维护系统**（Update，链上先于采集与 egui pass）：`dirty == false` 直接返回（Query 不迭代）；脏则全量走访 `EntityRow` 查询——relevant 表形状同 BRP `scene_tree`（entity → (label, children, has_parent)），根按实体号排序，DFS 只收**展开节点**成等高行；展开态账首见落默认（深度 < 2 展开），收尾 `retain` 清扫死实体；`dirty = false`。

**egui pass**：`ScrollArea::show_rows(ui, row_height, rows.len(), |ui, range|)`——egui 按滚动位置算可见 `range`（几十行），只构建 range 内的行：折叠形态画 `▾/▸ + label`（点击 = 展开/收起 + 选中），叶子画 selectable_label（点击 = 选中）。点选就地写 `SelectedEntity`（属性面板联动不变）；展开/收起在闭包外写 `expanded` + 置脏，下帧重建生效（≤2 帧）。

**稳态帧**：无事件 → 零重建；pass 只读 rows 切片构建可见行——每帧成本 = 窗口布局 + 可见行数（几十），与 15670 相关实体解耦。

## 3. 失败与边界路径

- 病态深树：`flatten_row` 深度 > 1024 停走（同 BRP `scene_tree` 栈防线的理由——栈溢出走 abort 是"禁 panic"下唯一不可兜底路径）。
- 悬空实体：重建从活查询出发，天然不含死实体；展开态账 `retain(seen)` 清扫。
- Name `Mut` 改写不触发重建（Insert 才触发）——本工程 Name 只在进场/spawn 写入，边界如实记录。
- 树窗私有（呈现态）：BRP `scene_tree` 保持一次性嵌套序列化，不吃 UI 缓存。

## 4. 代码改动简要解析（git show --stat 实数：2 文件 +226/−77 + 字形修正 1 行）

**扩展：**
- `overlay/windows/entity_tree_window.rs`（+226/−77，净主力）——新增 `TreeRow`/`EntityTreeState`（含 `rebuild`/`flatten_row`）+ 三观测器 + `maintain_entity_tree`；`EntityTreeWindow.show` 改 show_rows 视口裁剪渲染；退役 `draw_entity` 递归与 `header_text`（折叠头形态并入行渲染）。`EntityTreeData` SystemParam 保留（维护系统专用，pass 不再逐帧查询）。
- `overlay/framework/ui.rs`（+23/−…）——插件注册三观测器 + 维护系统入链（maintain → collect → pass → edits 链式）；`DebugUiParams` 的 `tree: EntityTreeData` 换 `tree_state: ResMut<EntityTreeState>`；`init_egui` 插缓存资源（默认脏 = 首帧立树）。
- 字形修正（+1/−1）——折叠箭头 U+25BE/U+25B8 → U+25BC/U+25B6（前者两款字体皆缺字）。

**取舍**：重建全量走（16k 实体 ~毫秒级）但只在事件帧发生，换稳态零迭代——"脏信号 + 全量重建"比增量维护简单一个量级，事件频率（spawn/despawn/换柄）远低于帧率，这笔账划算；视口裁剪交给 egui 原语，自管偏移未启用。
