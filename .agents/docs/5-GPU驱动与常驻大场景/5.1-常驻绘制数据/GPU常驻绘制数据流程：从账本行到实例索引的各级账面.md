# GPU常驻绘制数据流程：从账本行到实例索引的各级账面

> 5.1 功能流程导读（2026-10-15 收官）：一份数据从 ECS 到被 shader 按实例索引读到的全链路走读——三份存储按"写形状"分家的各级账面、四类变更（入行/移动/删除/换内容）的逐时刻对照、失败与稳态路径。文末附[代码改动简要解析](#六代码改动简要解析)（`git show --stat` 实数）。验收与判定线证据见[施工记录](5.1.1-5.1.6-常驻绘制数据施工记录：对象表、模型环与push退役.md)，本文不复述。

## 0. 一句话

**对象表记住"每个东西怎么画"（写一次），模型环记住"它现在在哪"（每帧增量），材质环记住"它长什么样"（每帧全量），push 只递一个槽号**——CPU 从"每帧递 96B 参数"变成"每帧递 4B 索引 + 只补改动"，GPU 因此第一次拿到了自己找数的能力（5.2 剔除、5.3 indirect 的前提）。

## 1. 各级账面全景（谁记什么、谁在何时写）

```
ECS 侧（跨帧账本 InstanceLedger，scene/mechanism/ledger.rs）
  rows: Entity → InstanceRow{mesh柄, material柄, model终值,
                             instance_slot(对象表下标), bound(记录身份)}
  dirty: 待盘点行集（发现侧入口，4.1 语义不变）
  freed_slots: 待交接旧槽（删行/换绑 → flush 消费进停尸房）
  model_dirty[2]: 模型环待写标记（按帧槽奇偶两套）
  stats: 扫描/处理/上传字节/槽位存量（BRP increment_stats 读数源）

GPU 存储侧（InstanceTables，vulkan/instance_table.rs）
  对象表  DEVICE_LOCAL 131072 槽 × 48B   写一次/槽；更新=新槽+停尸房
  模型环  HOST_VISIBLE ×2 帧槽 × 64B/槽  draw_frame 每帧增量重写
  材质环  HOST_VISIBLE ×2 帧槽 × 32B/槽  draw_frame 每帧全量重写
  实例槽 free list + 停尸房（排空=等全部帧槽 fence）
  材质槽 bump-only（AssetId → 槽号，跨帧稳定，不回收）

描述符（init 一次写入，运行期零更新）
  set0 bindless 双表（3.3 原样）｜set1 每帧 UBO（相机/灯光，4.x 形状不变）
  set2 b0 对象表 / b1 本帧模型环 / b2 本帧材质环（只读 storage）

着色器（debug_draw.wgsl）
  push 4B = 实例槽号 → VS: objects[slot] + models[slot]
                     → flat varying 材质槽号 → FS: materials[slot]
```

写纪律（hazard 三条闭合线，与责任边界契约同源）：

| 存储 | 写点 | 在飞安全凭什么 |
|---|---|---|
| 对象表 | flush 的 staging 批次（读侧=图形提交 ALL_COMMANDS 等票据；写侧=槽只来自 bump 或排空点归还） | 新槽在飞 draw 读不到（没人引用）；复用槽的旧读者已被排空点的帧侧等待收口 |
| 模型环 | draw_frame（`wait_for_slot` 之后） | 本帧槽上一轮提交已被帧槽 fence 等完——与帧 UBO/UI 顶点环同一条证据链 |
| 材质环 | 同上 | 同上 |

## 2. 新文件分工表（一层一句话）

| 文件 | 一句话职责 |
|---|---|
| `vulkan/instance_table.rs` | 三份 buffer + 槽位账 + set2 描述符 + 打包函数 + 保守包围盒纯函数（627 行，本段主体） |
| `vulkan/depth.rs` | DepthTarget/DEPTH_FORMAT 从 frames.rs 独立（3.6 P1 摊薄兑现位） |
| `driver/host/drawlist.rs` | DrawList 槽位枚举 + 双环写入（host 侧编排，摊薄新家） |
| `vulkan/mesh_convert.rs`（扩） | 顶点交错同一遍扫出局部 AABB |
| `vulkan/pool.rs`（扩） | MeshSlot 携带局部 AABB（commit/replace 增参） |
| `vulkan/resources.rs`（扩） | `BufferRole::DeviceStorage`（DEVICE_LOCAL storage 表角色） |

## 3. 逐时刻走读：四类变更

### 3.1 新对象入场（BRP spawn → 次帧上屏）

```
PostUpdate  update_ledger：行入账（slot=None, bound=None），dirty 置位
Last·前段   flush_uploads：
  ① 停尸房段——freed_slots 交接账消费 + 三家停尸房排空（同一帧侧等待）
  ② 行扫描——mesh 驻留 ✓ 材质容器在：ensure_material_slot(材质槽 bump)
     → alloc_object_slot（free list 优先）→ 旧槽(若有)进停尸房
     → 记录 48B 进 staging（同批）→ bound 落行 → mark_model_dirty（两套奇偶）
  ③ 合批提交 → 票据；票据值随 FrameDraw.wait_ticket 被图形提交在 GPU 侧等待
Last·draw_frame（wait_for_slot 之后）
  ④ DrawList 槽位枚举：行 → instance_slot + MeshSlot 引脚 → DrawCall{instance,引脚}
  ⑤ 模型环增量写（本帧奇偶集）+ 材质环全量写（可绘行材质从容器现读）
  ⑥ 录制：三集绑定 + 每 draw push 4B 槽号 + cmd_draw_indexed
  下帧：另一奇偶集的环补写（新槽的模型在两个帧槽各就位一次）
```

**槽号在两帧后完全"住熟"**：帧 N 的环 N%2 由 flush 后的 draw_frame 写，帧 N+1 的环 (N+1)%2 由下一帧补写——期间任何在飞 draw 读到的是自己那套环里的旧值，而旧值属于已消失的旧占用者（其 draw 在槽停尸房排空前已完成）。

### 3.2 移动（set_transform → 同帧环写、次帧上屏与 4.x 同拍）

PostUpdate 传播后 `update_ledger` 只写 `row.model`（不进 dirty，不过上传链）+ 双奇偶标记；draw_frame 取本帧奇偶集 → 64B 直写本帧模型环。**上传零字节、零票据**——移动成本 = 一行账本 + 64B host 写，静态场景两个环都零写入。

### 3.3 删除（despawn → 整树清账 → 槽停尸房 → 复用）

摘组件/despawn → 行清账、`instance_slot` 进 `freed_slots` → flush 把它送 `InstanceTables` 停尸房（**不进 free list**）→ 下帧 flush 开头 `drain_graveyard`：等全部帧槽 fence（在飞 draw 对旧记录的读取收口）→ 归还 free list。日志："对象表停尸房排空:归还 N 个实例槽"。复用槽的新记录写经票据，与新 draw 的读由同一条 ticket 依赖收口。

### 3.4 换内容（mesh Modified / 材质 mutate / 贴图换指）

- **mesh 内容变化**：失效凭据 → 行重脏 → flush 扫描时记录身份 `(mesh_id, ticket, material_id)` 比对失配（ticket 必换）→ 换槽重写（旧槽停尸房）。
- **材质参数/贴图槽变化**：4.2 的行重挂照跑，但 flush 扫描发现身份未变 → **零换槽零上传**；生效路径 = 材质环每帧全量重写从容器现读（10 万实例共享 5 材质的场景下，一次材质变更 = 5 个材质记录的重写而非 10 万个对象槽的链式 churn——这是材质内容不走 swap 的结构性理由）。

## 4. 失败与稳态

- **失败**：flush 内槽位/材质槽分配失败 = 容量耗尽（131072/65536 上界），Tier② 报错不截断；环写失败 = host-visible 契约被打破，Tier②。停尸房排空失败保留下帧重试（旧槽多活一帧，无悬空）。
- **稳态**：脏集空 → flush 空批早退（零盘点零上传保持）；DrawList 槽位枚举 O(在场行) 走访 + 材质环 O(唯一材质数) 重写 + 模型环零写入；push 每帧 4B×draw 数（与对象数线性，但这是索引标识不是参数——判定线 1 的"参数本体零推送"）。
- **5.2/5.3 的接口预留**：对象记录里的局部 AABB + set2 的 COMPUTE stage 声明是给 compute 剔除的；记录里的范围三参数是给 indirect 写参的。

## 5. 读数口

BRP `increment_stats` 的 `gpu_tables` 段：object_slots/object_capacity/object_graveyard/object_retired_total/material_slots/material_capacity/push_bytes_per_draw=4；上传日志的"对象记录 N 条(换槽重写)"与"实例槽 x/y(停尸房 z)、材质槽 m/n"。

## 6. 代码改动简要解析（`git show --stat` 9c78746d5，17 文件 +1321/−303）

**新增（3 件，925 行）**

- `vulkan/instance_table.rs`（+627）——三表 + 槽位账 + set2 + 打包 + 保守 AABB + 单测 4 枚；本段的全部新 Vulkan 结构。
- `vulkan/depth.rs`（+144）——DepthTarget/DEPTH_FORMAT 从 frames.rs 迁出（摊薄，纯移动无语义变化）。
- `driver/host/drawlist.rs`（+154）——槽位枚举 + 模型/材质双环写入（frame.rs 摊薄的新家）。

**扩展（8 件）**

- `driver/upload.rs`（+127/−21）——停尸房段接管 freed_slots、行扫描内记录身份比对与换槽、staging 排布记录段、计量与日志分列。
- `scene/mechanism/ledger.rs`（+80/−18）——行扩展（slot/bound）、freed_slots 交接账、model_dirty 双奇偶集、update_ledger 三入口适配。
- `driver/host/frame.rs`（+93/−68）——DrawList 组装外迁 drawlist、双环写入插入点、set2 接线；闸门/acquire/UBO 编排原样。
- `vulkan/resources.rs`（+14/−2）——`BufferRole::DeviceStorage`（usage/必需属性/优先属性三表各一行）。
- `vulkan/pool.rs`（+14/−8）——MeshSlot.local_aabb、commit/replace 增参（too_many_arguments 显式豁免）。
- `vulkan/mesh_convert.rs`（+11）——局部 AABB 同遍扫出。
- `remote/objects.rs`（+17/−4）——increment_stats 增 `gpu_tables` 段。
- `driver/host/init.rs`（+25/−10）——InstanceTables 进创建链/拆除链、管线建链增 set2 layout 参数。

**收缩/替换（6 件）**

- `vulkan/pipeline.rs`（+84 行中多为注释/日志，PushLayout+pack_push+PushData 约 −70 行）——push 冻结表 96B→4B，96B 打包器整体退役；layout 三集。
- `vulkan/frames.rs`（−161 净）——DepthTarget 迁出 + DrawCall 换 instance u32 + push 4B + 三集绑定。
- `examples/shaders/debug_draw.wgsl`（+59/−31）——三表读取、flat varying 材质槽、PushParams 缩 4B。
- `vulkan/mod.rs`、`driver/host/mod.rs`、`examples/upload_probe.rs`——重出口/挂线/新参适配。
