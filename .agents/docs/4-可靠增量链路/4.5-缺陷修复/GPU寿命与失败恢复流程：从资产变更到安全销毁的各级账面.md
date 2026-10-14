# GPU 资源寿命与失败恢复流程：从资产变更到安全销毁的各级账面

> 4.5 功能流程导读（plan-execute 收官定例）。本段没有引入全新数据流，但**三条既有链的规则被改写**：内容重传的槽位命运（F1）、采样器的所有权（F2）、销毁凭据（F3）与失败记账的入口（F4）。面向下一个读代码的人，按"谁在什么时刻改了哪本账"走读。代码改动实数：三批 feat 共 39 文件、799 insertions / 456 deletions（`git show --stat`：`005c6f594` 8 文件 +281/-176、`b5ce31b3d` 9 文件 +321/-220、`e05b03215` 22 文件 +197/-60，另 bevy_remote http.rs 一处 +14/-0 随第三批）。

## 一、账面全景（改动后的分工表）

| 账面 | 位置 | 4.5 改动 |
|---|---|---|
| `InstanceLedger`（实体行账） | `scene/mechanism/ledger.rs` | 统计口径订正：帧字段每帧刷新、`asset_invalidations_total` 累计只增、`asset_invalidations_current` 存量另列 |
| `AssetReadiness`（pending/failed 身份账） | `asset_readiness.rs` | **失败记账写入点统一**：note_failure/revive/clear 全部并入事件读侧单系统，本模块只剩结构定义 |
| `AssetInvalidations` / `AssetRetirements` | `asset_events.rs` | 事件纪律扩容：`Unused` 出清失败账（F8）、`Added` 无条件复活重挂（F9）、失败记账同系统先入（F4） |
| `BindlessTables.sampler_bodies`（**新账**） | `vulkan/descriptors.rs` | 采样器本体归表：槽号 → 表自建自持的 `VkSampler`（F2） |
| `BindlessTables.retired_*`（槽待回收账） | `vulkan/descriptors.rs` | 语义升级：retire_texture 成为内容重传的常规路径（F1），采样器本体随槽进待销毁账 |
| `ImageCache.resident / graveyard` | `vulkan/images.rs` | 停尸房条目 `(GpuImage, Ticket)`——票据随行（F3）；`swap_commit` 取代 `take`+rebind（F1） |
| `MeshPool.graveyard` | `vulkan/pool.rs` | 条目三元组化 `(+ticket)`，排空双条件（F3） |
| `AtlasGpu.current / graveyard` | `overlay/paint.rs` | 旧代接 retire 账 + 票据随行，排空挂 flush 统一排空点（F15） |
| `InitDrainGuard`（**新护栏**） | `driver/host/init.rs` | 初始化失败路径的 device_wait_idle 先于一切 Drop（F12） |

## 二、逐时刻走读

### 时刻 A：同柄内容变化（Modified → 重传）——4.2 链路的规则改写

1. `Assets::get_mut` 发 `Modified` → 次帧事件读侧收：身份进失效集 + `revive_*`（F6 复活前提）。
2. `flush_uploads` 盘点：失效凭据放行"已驻留要换内容"的行 → fresh 表 `(id, handle, replace=true)`。
3. 转换成功进批、`submit_batch` 拿新票据。
4. **槽位发布（改写点）**：`publish(view, key)` 分配**全新纹理槽**——不再 `rebind` 原槽重写（F1：在飞 draw 可能仍消费旧描述符）。新键首次出现时表自建 `VkSampler` 进 `sampler_bodies`（F2）。
5. `image_cache.swap_commit`：旧 GpuImage+旧票据 → 停尸房；旧纹理槽 → `retired_textures`；旧采样器槽仅当"新条目不同槽且无其余共享方"退场（本体随之进待销毁账）。新条目（新图、新票据、新槽）入 resident。
6. 过渡帧语义不变：在飞 draw 经旧槽读停尸房保活的旧图，新 draw 经新槽读新图——**旧图多活一帧的代价换覆盖安全**。

### 时刻 B：停尸房排空（每日 flush 步骤 0）

```
ImageCache.graveyard ─┐
MeshPool.graveyard  ──┼─→ 非空才等待：帧槽 fence（图形最后使用）
AtlasGpu.graveyard  ──┘   + 票据时间线信号量（上传完成，F3：最小化期间
                          flush 仍可提交 transfer 而无图形提交，帧 fence
                          完成不蕴含上传完成——旧图提前销毁的窗口堵死）
→ tables.drain_retired_slots：纹理/采样器槽回 free list；
  采样器本体此刻才 vkDestroySampler（F2：排空点=本体销毁点）
```

### 时刻 C：失败与复活（F4/F6/F8——单入口）

`propagate_asset_changes` 系统内固定次序：

```
1. LoadFailedEvent ×3 → note_failure（failed 集 + 计量 + warn）
2. AssetEvent 三路：
   Modified  → 失效集 + revive（修数据重试放行）
   Removed   → 回收账 + clear_failure
   Unused    → note_unused + clear_failure（F8：未到货失败只发 Unused，
              不出清则失败账只增不减）
   Added     → revive + 重挂集（F9：无条件——同身份卸载重插的行已"干净"，
              驻留已回收，无联动则新内容永不上传）
3. 早退判定纳入全部联动集（F7：材质复活不再被早退截断）
4. 帧窗口统计先落账（F10：空事件帧归零，不滞留）
```

原来的 `track_asset_failures` 独立系统不存在了——"复活清旗标"与"读旗标重挂"分属两系统、调度顺序不同结果不同（CPU 复现 dirty=1/0）的结构性分叉被合并抹平。

### 时刻 D：初始化失败（F12/F13——失败路径也守 D4）

`try_init_vulkan` 串链，任一 `?` 短路：逆序 Drop 中 `InitDrainGuard`（声明于 Uploader 之后）最先析构 → `device_wait_idle` → 其后 Uploader/表/GpuImage 析构时 transfer 已完成。`Swapchain::new` 内部 views/semaphores 半建失败：已建 view 逐个销毁 + swapchain 本体就地销毁——泄漏者占 hwnd → 重建恒报 `NATIVE_WINDOW_IN_USE` 的路径不存在。

## 三、失败与稳态路径对照

| 路径 | 改动前 | 改动后 |
|---|---|---|
| 稳态帧（零事件零脏行） | 不变 | 不变（统计仍每帧落账，成本零） |
| 换内容重传 | 原槽重写（在飞不安全） | 新槽发布 + 旧槽延迟回收 |
| 共享采样器卸载所有者 | 槽内句柄悬空 | 表持本体，槽退场才销毁 |
| 最小化期间加载→卸载 | 排空只等帧 fence | 双条件（+上传票据） |
| init 半建失败 | Uploader 带在飞析构（违 D4） | 护栏先排空再析构 |
| BRP 非法路径 | panic（绕过协议） | -32602 协议错误 |
| 组合 mutate 单项失败 | 前项已落改 | 整体拒绝零副作用 |
| 图集换代 | 旧代与表同寿（无界积聚） | retire + 统一排空点双条件销毁 |

## 四、新文件与改动分组

**无新文件**——本段全部是既有模块的规则改写：

| 分组 | 文件 | 要点 |
|---|---|---|
| F4/F6~F10（第一批） | `asset_events.rs`（单系统合并+事件纪律扩容）、`asset_readiness.rs`（系统退役、结构保留）、`upload.rs`（expel 出去重条件、dirty_rows 定案点）、`ledger.rs`（统计字段）、`remote_objects.rs`（stats 出口） | 失败记账单入口是本批结构性改动 |
| F1/F2/F3/F12~F15（第二批） | `descriptors.rs`（sampler_bodies 新账、publish 改签名、rebind 删除）、`images.rs`（swap_commit、票据停尸房、SamplerKey::create_sampler）、`pool.rs`（票据三元组）、`init.rs`（护栏）、`swapchain.rs`（全有或全无）、`frame.rs`（pending 置位）、`paint.rs`（图集接账） | 销毁凭据与所有权两条主线 |
| F16~F21+P3（第三批） | `remote.rs`/`mesh_convert.rs`/`material_hook.rs`/`camera.rs`/`http.rs`（bevy fork）/`swapchain.rs`/注释九处/小账七项 | 防御与同真，多为单点 |
