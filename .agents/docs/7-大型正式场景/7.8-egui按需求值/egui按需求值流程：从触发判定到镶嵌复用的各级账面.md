# egui按需求值流程：从触发判定到镶嵌复用的各级账面

> 7.8 段功能流程导读（面向下一个读代码的人）：egui pass 从"每帧必跑"改为"按需重求值 + 镶嵌复用"后，跨帧状态分住四处账面。验收对照归[施工记录](7.8.1-7.8.3-egui按需求值施工记录：基线取证、两层节拍化与交互零回退.md)，本文只讲机制走读。

## 0. 账面全景

| 账面 | 住处 | 记什么 | 谁写谁读 |
|---|---|---|---|
| 输入积压账 `EguiInput.pending` | framework/input.rs | 面板 pass 跳过帧攒下的 egui 事件（连续移动折叠成一条） | 桥每帧无条件排水写簿记；面板 pass 合并取走 |
| 帧产出 `EguiFrame` | framework/ui.rs | 两层 shapes + 各自代数 + ppp/屏幕尺寸 | pass 运行帧填充；绘制半边每帧消费 |
| 镶嵌缓存 `OverlayLogState.{panel,gizmo}_meshes` | framework/paint.rs | (shapes 代数, 镶嵌产物) 两层各一份 | 代数不等才重镶嵌；等则原样复用 |
| 门控账 `PassGate` | framework/ui.rs | 8 个跨帧 Local：首帧旗/拖拽旗/尺寸快照/相机旋转快照/gizmo 显隐快照/跳帧旗/egui 重绘请求旗/面板层清单（gizmo pass 补记可见性用） | run_egui_pass 独占读写 |

不变量：事件只被 egui 消费一次（面板 pass 吃，gizmo pass 永远空事件）；桥簿记（pointer_pos/buttons_down/修饰键）每帧无条件更新，与 pass 是否运行无关；纹理增量（`textures_delta`）只在 pass 帧产生、产生即就地折进图集镜像。

## 1. 逐时刻走读（Update 的 run_egui_pass，五类帧）

**① 稳态帧**（无输入、快照未到期）：`fps.tick` 照跑 → 组 RawInput（桥排水+簿记）→ 事件为空 → panel_due 假 → `stash_events`（无事件可攒）→ gizmo_due 假（旋转没变）→ **零 pass**。`EguiFrame` 两层代数不动；绘制半边 `tessellate_layer` 看代数相等 → 直接复用镶嵌缓存 → 顶点环照写 → 画面原样。成本：帧墙钟里 egui 侧只剩环写入。

**② 输入帧**（按键/滚轮/焦点/按键释放，或悬于 UI 的移动）：事件非空且面板相关 → panel_due 真 → 取走积压（旧在前）+ 本帧事件 → 若刚从跳帧回来，尾部补发当前位置 → begin_pass → 总控/监控/统计/树窗 → 插件钩子 end → shapes 落 `frame.panel`，代数 +1 → 读 `repaint_delay`，为 0 则 `force_next` 置位。gizmo 层只有相机也变了才跟一趟。

**③ 快照帧**（0.5s 到期）：fps 快照刷新 → 监控历史采样 + 分位缓存重算（每帧底座部分）→ snapshot_ticked 置 panel_due → 同②走一遍（面板数字这才更新）。每秒两跳，对应基线/优化后分布里的 max 值帧。

**④ 环视帧**（右键拖拽场景，指针不在 UI 上）：CursorMoved 事件流每帧到 → 移动事件 over_ui 判假 → panel_events 假 → 面板跳过（事件进积压，折叠成最新位置）；相机旋转变 → gizmo_due 真 → 空 RawInput 子 pass 只画 gizmo → `frame.gizmo` 代数 +1。绘制半边：面板层复用缓存、gizmo 层重镶嵌——面板冻结、指示器实时。

**⑤ 交互拖拽帧**（按住拖选/拖窗，拖拽始于 UI）：press 帧置 `ui_drag` 真 → 此后即使指针拖出窗外，移动事件也触发面板（拖拽不冻结）→ 全键释放帧清 `ui_drag`。同帧 `_ => panel_events` 兜底让按钮事件本身必然触发。

## 2. 绘制半边的复用判定（draw_frame 的 paint_overlay）

镶嵌入口 `tessellate_layer(ctx, layer, stored, ppp)`：`layer.generation != stored.0` → `tessellate(mem::take(shapes))` 并落缓存；相等 → 原样复用 `stored.1`。两层各判各的，随后 `flatten_meshes` 按面板先、gizmo 后的画家序摊平进顶点/索引流（gizmo 压顶与单 pass 时代一致）。ppp 变化必经尺寸变化触发两层 pass（代数必 +1），故代数相等蕴含 ppp 相等，缓存不会拿错 dpi 的产物。

## 3. 失败与稳态路径

- **pass 从未跑过**（首帧前）：两层代数 0、shapes 空、缓存空 → `flatten` 产零 draw → 本帧无 UI，与旧行为一致。
- **显隐切换**：`windows_open` 写点在 pass 内（checkbox/[×]），出口对比进 force_next，下帧重建即生效；gizmo 关闭走 shapes 清空、代数照推进。
- **BRP 写选中实体/外部改实体**：`SelectedEntity`/`EntityTreeState` Changed → 下帧面板 due；点选链接力一帧内可见（实测）。
- **egui 动画**：靠 repaint_delay==0 的 force_next 续帧，动画跑完自动回落稳态。
- **层可见性补记（面板消失修复）**：egui 按"上一个 pass 画过谁"记账窗口可见性——gizmo 子 pass 不画窗口会让面板层从账面蒸发，下个面板 pass 被视为"窗口刚重现"而重触发淡入（移动中面板只剩半透明残影的根因）。修复：面板 pass 出口记 `visible_layer_ids` 进 PassGate.panel_layers，gizmo pass begin 后逐层 `move_to_top` 补记，换道即保留。新增子 pass 一律照此补记。

## 4. 代码改动简要解析（`git show --stat`：3 文件，+423/−156）

- `overlay/framework/input.rs`（+35）：`EguiInput` 增 `pending` 积压账与 `pointer_pos`/`any_button_down`/`stash_events`/`drain_pending` 四件——簿记无条件更新语义不变，"欠账补报"纪律原样。
- `overlay/framework/ui.rs`（+401/−156）：`EguiFrame` 拆两层 `EguiLayer`（shapes+代数）；`PassGate` SystemParam 束；`run_egui_pass` 重排为"节拍底座 → 排水 → 触发判定 → 面板 pass / gizmo pass → 落账"；插件钩子抽 `plugin_hooks_begin/end` 供两 pass 配对。
- `overlay/framework/paint.rs`（+143/−56）：`OverlayLogState` 增两层镶嵌缓存；镶嵌改 `tessellate_layer`（代数判等），摊平拆 `flatten_meshes`（面板先 gizmo 后）。绘制链其余（图集整传/环写入/push 契约）零改动。
