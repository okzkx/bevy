# 异步就绪流程：从一次 load 请求到就绪上屏的各级账面

对应[段 README](README.md) 与[施工计划](施工计划：异步就绪与失败取消.md)。4.3 给上传链补上了"资产还没到货"这条时间线的账面——本文面向下一个读代码的人，走读一次运行期 `load_mesh` 请求从发起到上屏（或失败/取消）的全链路：每一级账面在哪、谁写谁读、失败与稳态路径长什么样。机制取舍与验收证据见施工记录，不在此复述。

## 1. 各级账面全景

一次"引用了尚未到货资产的实体"在链路上会经过五级账面，前四级 4.1/4.2 已有，第五级（readiness）是本段新增：

| 级 | 账面 | 粒度 | 谁写 | 谁读 |
|---|---|---|---|---|
| ① World | `Mesh3d` 组件 | 实体 | 任何写入方（BRP/UI/代码） | update_ledger（Changed 通道） |
| ② 账本 | `InstanceLedger` 行 + 脏集 | 实体粒度 | update_ledger 三入口 | flush_uploads（脏行盘点）、DrawList（rows） |
| ③ 失效凭据 | `AssetInvalidations`（mesh/image 身份集） | 资产粒度·负向（已驻留要换内容） | 4.2 事件读侧（Modified） | flush 判定放行（"查无**或失效**才传"） |
| ④ **就绪账面** | `AssetReadiness`（pending 身份 → 等待帧数 + failed 集） | 资产粒度·正向（未驻留在等） | flush（mark/arrive/expel/reconcile）+ 失败读侧（note_failure/revive） | flush 三态判定、`increment_stats` 读数 |
| ⑤ GPU 驻留 | `MeshPool.resident` / `ImageCache.resident` + 提交票据 | 资产粒度 | flush（提交成功才 commit） | DrawList（现查）、图形侧（票据信号量等待） |

关键分层：②是实体粒度、③④⑤是资产粒度；③与④生命周期相反（失效="有内容要换"，pending="没内容在等"）；⑤的"上传完成才可使用"由 3.2.4 票据承担——readiness 是纯 ECS 侧账面，零 Vulkan 代码。

## 2. 新文件与改动分工（一层一句话）

- `scene/mechanism/assets/asset_readiness.rs`（新增）——就绪账面本体：pending/failed 两本账 + 失败读侧系统 + ReadinessStats 计量。
- `driver/upload.rs`——盘点段从"驻留与否"二态改"完成/失败/pending"三态；转换段 mark_pending、commit 段 arrive、帧末 reconcile（含空批早退路径）。
- `scene/mechanism/editing/object_ops.rs`——加载操作本体三件（换柄加载/克隆加载/贴图换槽），可复用层。
- `remote/assets.rs` + `remote/mod.rs`——BRP 薄壳 `load_mesh`/`load_image` 与注册。
- `remote/objects.rs`——`increment_stats` 扩 readiness 读数段。
- `main.rs`——`AshReadinessPlugin` 插入插件清单（资产事件读侧之后）。
- `tools/brp.py`——`load-mesh`/`load-image` 子命令封装。

## 3. 逐时刻走读：一次 load_mesh 的完整生命周期

以 `brp.py load-mesh --path "models/x.gltf#Mesh0/Primitive0" --source <源>` 为例：

**T0 帧（RemoteLast，BRP handler）**：`AssetServer.load` 立即返回**加载中柄**（AssetId 已定型，锚身份非内容）；克隆源结构 + 把 `Mesh3d` 柄换成加载中柄。返回读回 `{entity, mesh: AssetId, state: "loading"}`——命令不等到货。

**T1 帧（PostUpdate）**：update_ledger 的 Changed 通道见新 `Mesh3d` → 账本入行 + 置脏。失败读侧同节拍扫 `AssetLoadFailedEvent`（此时无事）。

**T1~Tn 帧（Last，flush_uploads）**：盘点脏行，mesh 腿三态判定：
- `pool.resident` 查无且身份不在 failed 集 → **pending 腿**：`Assets::get` 查无 → `mark_pending`（入账，等待帧数从 0 起）→ 不进本批。行保持脏标。
- 之后每帧重复：`reconcile` 给仍被引用的 pending 身份等待 +1——**这就是"等待帧数"的账**（T1~Tn 每帧一行，跨帧累计）。
- 画侧同帧：DrawList 读账本行，mesh 未驻留 → 整行暂缓（"上传链自愈后消失"日志），画面不读空槽。

**Tn 帧（资产到货）**：AssetServer 任务完成，`Assets::get` 命中 → flush 转换成功 → 容量保证 → 合批提交 → **提交成功才 commit/arrive**：驻留登记 + pending 身份出账（等待帧数计入 `arrivals`/`last_arrival_wait`）。次帧 DrawList 见驻留 → 上屏。

**失败路径**：路径不存在等错误在**延迟窗口后**才被 AssetServer 探测到（实测 ~0.4s=25 帧，期间身份如实 pending）。失败读侧收到 `AssetLoadFailedEvent` → 进 failed 集 + warn 可见；下一帧 flush 盘点该行：mesh 腿=失败腿，若行无其余 pending 腿 → 出队（清脏标，`failed_rows_cleared` 计量）。此后该身份不再进重试批——**僵尸重试队列断根**；`Added`/`Modified` 事件是唯一复活口（重载/修数据）。失败身份的行留在账本上，DrawList 持续暂缓（实体在树可见、画面无此对象）。

**取消路径**：pending 期间引用行 despawn（或换柄改指）→ 账本清行 → 次 reconcile 时该身份不在 marked 集 → 出账计 `cancelled_total`。资产**之后**才到货？无所谓——没有脏行就没有上传凭据，flush 结构上不会为它转一个字节（实测：64MB 资产在引用者消失后到货，bytes 纹丝不动）。

**稳态路径**：全部驻留后 pending 集空、脏集空，flush 每帧零盘点零上传（4.1 基准不变）；readiness 只是多了一本静着的账（读数全零）。

## 4. 失败与稳态路径对照（一笔账的三种结局）

| 结局 | pending 账 | 脏行 | 计量 | 画面 |
|---|---|---|---|---|
| 到货（arrive） | 出账 | 清标 | arrivals+1、等待帧数入账 | 到货帧后上屏 |
| 失败（expel/note_failure） | 出账 | 出队（无 pending 腿时） | failures/failed_rows_cleared | 永不上屏（直到复活） |
| 取消（reconcile 出账） | 出账 | 已随 despawn 清行 | cancelled+1 | 从未出现 |

## 5. 代码改动简要解析（git show --stat 实数，feat 07325dc87，10 文件 +639/−34）

**新增**：
- `scene/mechanism/assets/asset_readiness.rs`（+292）——`AssetReadiness` 资源（pending 两 map + failed 三 set + ReadinessStats 十项）、mark/arrive/expel/revive/reconcile 六个写口、失败读侧系统 `track_asset_failures`（三类 `AssetLoadFailedEvent` + 三类 `AssetEvent` 复活口）与 `AshReadinessPlugin`。取舍：失败读侧只记账不裁决，行出队归 flush 整行状态判定——避免读侧各自为政清行（失败腿+pending 腿并存的行不能出队）。

**扩展**：
- `driver/upload.rs`（+85/−34 中的主体）——盘点段三态判定（完成/失败/pending 逐腿）；转换段查无 `mark_pending`、拒绝 `expel`；commit 段 `arrive`；帧末 `reconcile`（空批早退路径也走，等待帧数照常累计）；日志补"失败出队行"计数。取舍：reconcile 放收尾统一做而非逐事件做——marked 集是"当帧仍有引用"的唯一事实，逐事件做会在跨帧时序上误判取消。
- `scene/mechanism/editing/object_ops.rs`（+98）——`load_mesh_rebind`/`spawn_with_loaded_mesh`/`load_image_slot` 三件加载本体 + `MaterialAssetMissing` 错误变体；`load` 收 `AssetPath<'static>`，`&str` 须转 owned。
- `remote_assets.rs`（+94）——`load_mesh_cmd`（entity|source 二选一）/`load_image_cmd` 薄壳；复用 remote_objects 的 `object_op_error`（升 pub(crate)，加新变体映射）。
- `remote_objects.rs`（+36）——`increment_stats` 加 readiness 段（pending/等待/到货/失败/取消十项分列）。
- `remote.rs`（+11）、`main.rs`（+5）、`scene/{mod.rs,mechanism/mod.rs}`（+11）——注册与重出口。
- `tools/brp.py`（+41）——`load-mesh`/`load-image` 子命令 + 文档串 4.3 段。

**零改动**：账本（`ledger.rs`）零碰——pending 是资产粒度账，不进实体行；`AssetInvalidations` 零碰（4.2 凭据语义不变）；Vulkan 侧（uploader/pool/images/frames）零碰——票据机制 3.2.4 现成，本段零新增 Vulkan 代码方向兑现。
