# DrawList 组装流程导读：从账本行到在场环的增量维护

> 7.9 段引入"组装缓存"新机制后，写给下一个读代码的人：从一行的生老病死，到它出现在 GPU 命令宇宙里的完整时刻表。验收口径见[施工记录](7.9.1-7.9.4-DrawList组装与双环增量施工记录：账本侧组装缓存、全静闸门与对照模式.md)。

## 0. 一句话

`drawlist::assemble` 曾经每帧对全部账本行做一次 O(rows) 可绘判定（zero_day 7159 行 7.4ms/帧），现在这袋判定结果**住在账本里增量维护**：行状态没变的帧零走访、零产物重建；变了才重估那一行。

## 1. 账面全景：谁持有哪份真值

| 账面 | 位置 | 内容 | 写者 | 读者 |
|---|---|---|---|---|
| `InstanceLedger` 行表 | scene/mechanism/ledger.rs | 每实体一行（柄/矩阵/实例槽/记录身份） | `update_ledger`（发现）、`flush_uploads`（槽位） | 全链 |
| `AssemblyCache`（7.9 新增） | 账本字段 | 可绘行集合（Entity→槽号/材质/alpha）+ 材质引用计数 + 待重估集 + 产物 Vec | `assembly_update`（帧循环触发，判定闭包由 drawlist 注入） | draw_frame 组装段→帧尾 |
| `lod_dirty`/`model_dirty` | 账本字段 | 模型环/LOD 表环的奇偶待写集 | 换槽/矩阵变化/档位到货/淘汰 | `write_rings` |
| GPU 在场环 | InstanceTables | 本帧命令宇宙（槽号列表，**每帧照写**——环按帧槽双缓冲，快照语义不可省） | `write_presence_ring` | build_indirect |

关键分工：**账本持有"行的世界侧事实"，组装缓存持有"这些事实的可绘投影"**。投影的全部输入变化都必须经过账本的行状态变化——这是增量合法性（"行宇宙不变 → 投影不变"）的物理保证，也是不做第二套变更发现的原因。

## 2. 组装缓存的内部形状（AssemblyCache）

```
AssemblyCache（账本字段，7.9）
├─ drawable: BTreeMap<Entity, (槽号, 材质身份, alpha<1)>   ← 可绘行集合，Entity 迭代序
├─ material_refs: HashMap<材质身份, 引用行数>              ← 材质集的增量源（0 出表）
├─ stale: BTreeSet<Entity>                                ← 待重估集（各变更点 insert）
├─ slots / materials: Vec（产物缓存，变化帧才重建）
├─ seeded / rebuild_all / last_floor_anchor / last_lod_generation  ← 换形键
└─ rebuilds_total / reevals_total / drained_rows_total           ← 计量（draw_stats 读）
```

**换形（全量重建）只有三个触发**：首帧播种、`floor_anchor` 开关翻转、`LodGroups.register` 换代（register 递增换代号）。其余全部走行级重估：`remove 旧行 → eval 当前行 → insert 新态`，引用计数与 alpha 计数随进出收账。

## 3. 逐时刻走读（谁在什么时候标 stale）

一行的可绘判定 = `实例槽在账 ∧ 锚（按 6.5 开关解析）在 MeshPool 驻留 ∧ 材质在容器`。三个输入各自的生老病死，与重估标记的落点：

| 时刻 | 变化 | 重估标记落点 | 生效帧 |
|---|---|---|---|
| spawn primitive | `update_ledger` ② 入行（instance_slot=None） | `mark_assembly_stale`（行插入点） | 同帧 draw_frame（行不可绘，暂缓） |
| 资产到货→换槽 | `flush_uploads` 脏行盘点 → `set_instance_slot` | flush 脏行循环逐行标记 + `set_instance_slot` 内部标记 | 同帧 draw_frame（进入可绘集） |
| transform 移动 | `update_ledger` ③ 只写 model + `mark_model_dirty` | **不标记**（可绘判定不含矩阵） | 模型环奇偶补写，组装零成本 |
| 材质内容 mutate（BRP） | `mutate_asset` 落写（RemoteLast） | **写入点直标** `rows_with_material` 反查 | 同帧 draw_frame（alpha 计数收敛） |
| 材质内容 mutate（事件链兜底） | 资产事件读侧隔帧可读 → `mark_dirty` | `mark_dirty` 内部标记 | 写点标记缺失时的兜底路径 |
| 驻留淘汰（预算/档位） | `residency_tick` → `retirements` → 下帧 flush 开头 `pool.retire` | flush 出账点 `rows_with_mesh` + `base_of` 反查（锚可能是基准或保底档，两个候选集都标） | 同帧 draw_frame（退场，暂缓） |
| 资产离容器（Removed） | 资产事件读侧 | `rows_with_material` 反查（材质 Removed 原本不重挂脏标，7.9 新增组装联动） | 同帧 draw_frame |
| despawn/摘组件 | `update_ledger` ① 清行 | 行删除点标记（重估发现行已清 = 只退场收账） | 同帧 draw_frame |
| 锚开关翻转 | `residency_tick` 下降沿全行置脏（既有）| `mark_dirty` 内部标记 + 组装侧键比对双保险 | 同帧全量重建 |
| LOD 组登记 | `spawn_lod_ladder` → `register` | 换代号跳变 → 全量重建 | 同帧 |

## 4. 帧循环里的借用纪律（draw_frame 步骤 3/4a）

```
drawlist::assemble_update(&mut ledger, pool, ...)   ← 排空重估集（&mut 收口）
verify.enabled → verify_assembly(&ledger, ...)      ← 对照模式：全量参照 vs 缓存
ledger.take_model_dirty / take_lod_dirty            ← 奇偶集先行取走（&mut 收口）
let assembly = ledger.assembly_view()               ← 此后全程共享借用
drawlist::write_rings(tables, &ledger, ..., &assembly, ...)
  ├─ write_model_ring(model_dirty 切片 + &ledger.row 查行)
  ├─ write_material_ring(assembly.materials)        ← 全量重写（7.9 裁决留全量）
  ├─ write_presence_ring(assembly.slots)            ← 每帧照写（环双缓冲）
  └─ write_lod_table_ring(lod_dirty 切片 + &ledger)
```

`AssemblyView` 是账本组装产物的只读借出（slots/materials/alpha_override）——取视图前收口全部 `&mut` 消费点，之后环写入与 cull/occlusion/lod 诊断读数零可变访问。

## 5. 失败与稳态路径

- **稳态帧**（行宇宙不变）：`stale` 空 → 零 eval、零 Vec 重建；成本只剩在场环的本帧 memcpy（环双缓冲语义，与组装无关）。
- **加载期**：行入账→资产到货→换槽，每行恰好两次重估（入行时不可绘、到货时可绘）——zero_day 全程 reevals 14318 ≈ 7159×2。
- **驻留 thrash**（预算边界淘汰拉锯）：eject→flush 出账→重估退场；冷却窗后 radius/需求重请求→置脏→flush→重估回归。冷却表**非空时 tick 闸门必须放行**（到期重请求是时间驱动的可观测转移——见施工记录首钉）。
- **材质离容器**：行失去可绘腿 → 重估退场（暂缓）；重插 Added → 事件链 mark_dirty → flush → 重估回归（4.4 复活联动同一条通道）。

## 6. BRP 读数面（新增）

`draw_stats` 新增两块：
- `assembly`：rebuilds_total / reevals_total / drained_rows_total / last_drain_rows——重建与重估的对账读数（零走访帧的判定证据）。
- `verify`：enabled / frames_compared_total / mismatches_total / last_mismatch——对照模式（`drawlist_verify_set` 开关）的判定线 2 证据通道，只进计量不驱动绘制。

## 7. 代码改动解析（git show --stat 实数：12 文件 +777/−103）

| 文件 | 改动要点 |
|---|---|
| scene/mechanism/ledger.rs（+362） | AssemblyCache 结构 + `assembly_update`（重建/重估/产物重建）+ `AssemblyView` + 变更点钩子（行增删/换绑/mark_dirty/set_instance_slot）+ `rows_with_material` + 单测 5 条 |
| driver/host/drawlist.rs（+236 改） | `assemble` 全量走访退役 → `drawability_eval` 闭包 + `assemble_update` 包装 + `assemble_reference`（对照真值面）+ `verify_assembly` + `DrawListVerify` 账面 + `write_rings` 共享借用签名 |
| driver/host/frame.rs（+47） | 步骤 3 改增量维护 + verify 钩子 + 奇偶集先取 + `FrameInput.verify` 束 |
| scene/mechanism/residency.rs（+109） | 全静闸门（TickGateKey 键 + 五条件 + 冷却表空放行规则）+ `TickResources` 参数束（16 上限对策）|
| driver/upload.rs（+20） | 脏行循环逐行标记 + 驻留出账反查标记（基准 + 保底档两候选集）|
| scene/mechanism/asset_events.rs（+10） | 材质 Removed → 引用行组装标记（7.9 新增联动）|
| scene/mechanism/asset_ops.rs（+16） | mutate 写入点直标（RemoteLast 内容写入不绕隔帧事件链）|
| scene/mechanism/lod_registry.rs（+12） | register 换代号（组装缓存换形触发键）|
| scene/mechanism/mod.rs、driver/host/mod.rs（+8） | AssemblyView 再导出 + DrawListVerify 资源注册 |
| remote/draw.rs、remote/mod.rs（+60） | draw_stats 组装/verify 块 + `drawlist_verify_set` 方法注册 |
