# indirect闭环流程：从账本行到一次绘制提交的各级账面

> 5.3 收官定例（流程导读，无任务序号前缀）：本段把"画多少、画哪个"的最后一步交给 GPU——绘制调用数从此与在场对象数解耦。面向下一个读代码的人，讲清每一级的账面、每帧的时刻表、以及"谁写谁读、靠什么 fence 免冲突"。验收复述不在本文（归[施工记录](5.3.1-5.3.5-indirect闭环施工记录：能力闸门、命令组装与图形消费.md)）。

## 0. 新文件分工表

| 文件 | 一句话职责 |
|---|---|
| `shaders/build_indirect.wgsl` | 命令组装 compute：逐在场行读对象表三引脚 + 标记 → 写一条 20B `VkDrawIndexedIndirectCommand`（不可见 `instanceCount=0`，`firstInstance`=槽号） |
| `src/vulkan/indirect.rs` | 组装管线（IndirectPass：set2 单集 + 4B push count）+ dispatch/屏障录制 + 诊断账面（IndirectStats）+ count 上界执法（maxDrawIndirectCount） |
| `src/remote/draw.rs` | BRP `ash_renderer/draw_stats`：绘制调用数/命令条数/零实例条数读数（只读，诊断标注） |
| `shaders/debug_draw.wgsl`（改） | `push.instance` → `@builtin(instance_index)`（firstInstance 携带槽号），图形 push 通道删除 |
| `src/vulkan/instance_table.rs`（改） | 环家族 4→6：在场环（b4，HOST_VISIBLE）+ 命令环（b5，DEVICE_LOCAL 新角色） |
| `src/vulkan/frames.rs`（改） | 场景段逐 draw 循环退役 → 一次 `cmd_draw_indexed_indirect`；组装 dispatch 记录位 |

## 1. 各级账面全景（一帧的静态结构）

```
InstanceLedger（账本，CPU）
  └─ drawlist::assemble ──► DrawAssembly { slots, materials, pending_rows }
       │                        （slots = 可绘行槽号列表，暂缓行不进）
       ├─► 在场环 presence_rings[帧槽]（HOST_VISIBLE，全量重写）── set2 b4
       ├─► 材质环 / 模型环（5.1 语义不变）          ── set2 b1/b2
       └─► IndirectStats（诊断快照，只进计量）

对象表 object_buffer（DEVICE_LOCAL，48B/槽写一次）── set2 b0
标记环 mark_rings[帧槽]（HOST_VISIBLE，compute 写 / 关闭时 host 填全 1）── set2 b3
命令环 command_rings[帧槽]（DEVICE_LOCAL 新角色 IndirectCommands）── set2 b5
```

写形状三分法下的第五、第六族：**在场环 = 全量重写**（与标记环同形状，量 = 可绘行数）、**命令环 = 全量重写**（纯设备侧，量 = 在场行数）。都没有分配语义、不进账本不进失效链——游标归属定案（2026-10-15）的直接应用。

## 2. 逐时刻走读（一帧 = draw_frame）

1. **wait_for_slot**：帧槽 fence 过——此后对帧槽环（帧 UBO/模型/材质/标记/在场）的宿主写全在此纪律内。
2. **assemble**：账本 O(N) 走访 → 可绘行槽号 `slots`（实例槽缺/mesh 未驻留/材质缺席 = 暂缓，不进列表）。
3. **write_rings**：模型增量 + 材质全量 + **在场环全量**（本段新增）写本帧槽。
4. **cull 半边**（5.2 语义不变）：开关 on → cull dispatch 输入（112B push 六平面 + bound）；off → host 把标记环本帧槽填全 1。诊断读标记。
5. **indirect 半边**：`indirect_pass.draw(slots.len())` → IndirectDraw（4B push count + 组数）；IndirectStats 刷新（draw_calls_recorded/command_count/zero_instance_count = 行数 − 可见数）。
6. **record_frame（渲染进场之前，两个 dispatch 顺序执行）**：
   - cull dispatch → 屏障 A（5.2 预置：`SHADER_WRITE → SHADER_READ @ VERTEX|COMPUTE`）——标记写 → 组装读；
   - build_indirect dispatch（读 b0/b3/b4，写 b5）→ 屏障 B（本段新增：`SHADER_WRITE → INDIRECT_COMMAND_READ @ DRAW_INDIRECT`）——命令写 → 绘制读。
7. **dynamic rendering**：`count > 0` 时绑管线 + set0/1/2 + 池，**一次** `cmd_draw_indexed_indirect(命令环, 0, count, 20)`；count = 0 整段跳过（空场）。GPU 侧逐命令：instanceCount=0 的命令零顶点工作；instanceCount=1 的命令里 `instance_index == firstInstance == 槽号`，VS 按它读 set2。
8. overlay UI 段照旧（自带 push/管线，与本链无关）→ 出场 → submit（等 acquire + 上传票据）→ present → advance。

## 3. 数据依赖与屏障（为什么恰好两条屏障）

| 写方 | 读方 | 收口 |
|---|---|---|
| host 写在场环/模型环/材质环/标记环（关闭时） | 本帧 compute/图形 | `wait_for_slot` fence（提交侧证据，宿主写无 barrier 执法点——环+fence 组合拳） |
| cull compute 写标记 | build_indirect compute 读 | 屏障 A（5.2 预置，dst 含 COMPUTE） |
| transfer 写对象表 | build_indirect/VS 读 | 上传票据等待（帧提交 ALL_COMMANDS，5.1 契约） |
| build_indirect compute 写命令 | `cmd_draw_indexed_indirect` 读 | 屏障 B（本段，dst `DRAW_INDIRECT`）——**indirect 的读者不是 shader stage**，dst 必须开到 DRAW_INDIRECT 阶段、access 用 INDIRECT_COMMAND_READ |

## 4. 失败与稳态路径

- **稳态**（对象集不变、相机不动）：零盘点零上传零模型环写；每帧成本 = assemble 走访 + 在场环/材质环全量重写 + 两次 dispatch + 一次 indirect 提交。绘制调用数恒 1（或空场 0）。
- **暂缓行**：不进在场环 = 自动不画，自愈后次帧回来（4.x 语义原样保留，GPU 侧零特判）。
- **despawn**：行出账 → 槽进停尸房 → 排空后回 free list；在场环不列 = 幽灵在源头关闭（比"固定槽位 + 标记补丁"的起步口径少一类正确性补丁）。
- **count 超限**：在场行数 > maxDrawIndirectCount 显式报错（`IndirectPass::draw`），不截断。
- **错误分流**：环写失败/dispatch 输入失败/录制失败 → `or_fatal` 冒泡统一通道（Tier② 优雅退出），与帧循环既有纪律同链。

## 5. 代码改动简要解析（git show 实数，见 goal/5.3 分组提交）

| 分组 | 文件 | 要点 |
|---|---|---|
| feat·闸门 | `vulkan/context.rs`（+27） | base features 首次启用：支持查询快照 → 硬校验 → `enabled_features(multi_draw_indirect)` → 就绪日志钉支持→启用与 maxDrawIndirectCount；`Context::limits()` 查询口 |
| feat·命令环 | `vulkan/resources.rs`（+18）、`vulkan/instance_table.rs`（+123/−58） | 新 `BufferRole::IndirectCommands`（STORAGE+INDIRECT_BUFFER、DEVICE_LOCAL）；InstanceTables 第五/六族 + set2 b4/b5 + 访问器（presence_ring/indirect_buffer）+ `INDIRECT_COMMAND_SIZE`/`PRESENCE_ENTRY_SIZE` |
| feat·组装 pass | `vulkan/indirect.rs`（新 264 行）、`build_indirect.wgsl`（新）、`vulkan/mod.rs` | IndirectPass（照 CullPass 形状）+ record（dispatch+屏障 B）+ IndirectStats + 2 单测；shader 逐在场行组装五字段命令 |
| feat·消费 | `vulkan/frames.rs`（+89/−73）、`vulkan/pipeline.rs`（+28/−28 净删）、`debug_draw.wgsl` | FrameDraw `draws→(indirect_count, indirect_buffer, indirect)`；场景段一次 indirect 提交；图形 push range 与 `PUSH_CONSTANTS_SIZE` 退役；VS 改 `@builtin(instance_index)` |
| feat·宿主 | `driver/host/drawlist.rs`（+59）、`frame.rs`（+56）、`init.rs`、`mod.rs` | `DrawCall`→`slots` + 在场环写入；draw_frame 组装 indirect dispatch + IndirectStats 刷新；IndirectPass 进 init 链/teardown 序 |
| feat·通道 | `overlay/windows/debug_window.rs`、`framework/ui.rs`、`remote/draw.rs`（新）、`remote/mod.rs`、`remote/objects.rs`、`tools/brp.py` | egui indirect 读数行；BRP `draw_stats` 方法族 + 十九子命令；`push_bytes_per_draw` 4→0 订正 |
| docs | `.agents/docs/5-GPU驱动与常驻大场景/5.3-indirect闭环/*` + 路线图 | 施工记录 + 本文 + 状态三处同步 |

合计 16 改 + 4 新（src 345 insertions / 147 deletions，不含 docs）。

## 6. 与 5.2 流程导读的接缝

5.2 的剔除链终点是"标记环（产而未用）"；本文第 2 节第 6-7 步就是那份标记的第一个真读者。5.4 常驻大场景进来时：在场环写侧的 O(N) 走访与 chunk 组织的关系、命令条数随 chunk 增删的形状，是下一段的第一个问题。
