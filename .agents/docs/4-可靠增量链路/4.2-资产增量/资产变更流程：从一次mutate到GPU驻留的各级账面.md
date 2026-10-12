# 资产变更流程：从一次 mutate 到 GPU 驻留的各级账面

> 4.2 配套导读（机制定案见[施工记录](4.2.1-4.2.5-资产增量施工记录：事件读侧、失效联动与同柄重传.md)，设计取舍见[讲解篇](AssetId变更语义：两条变更通道、Id稳定性取舍与驻留失效实现.md)）。回答一个问题：**资产内容变了，每一层账面在什么时刻发生什么**。

## 0. 一张图：六个账面层

```
BRP/egui/游戏代码
   │ Assets::get_mut(同 AssetId 改内容)          ← asset_ops.rs（写入封装）
   ▼
Assets 容器 ──发出── AssetEvent::Modified        ← bevy_asset（引擎自带）
   │
   │ 次帧 PostUpdate
   ▼
asset_events.rs（读侧发现）
   ├─ AssetInvalidations：mesh/image 身份进失效集（凭据）
   └─ InstanceLedger：引用该资产的实体行重挂脏标
   │
   │ 同帧 Last
   ▼
flush_uploads（upload.rs，写侧编排）
   ├─ 盘点脏行：判定"查无 **或失效**" → 换内容身份进重传批
   ├─ mesh：新区间上传 → MeshPool::replace 原子换槽（旧区间孤儿化）
   ├─ image：旧图进 ImageCache 停尸房 → 新图上传 → BindlessTables::rebind 原槽重写
   └─ 凭据销账（成功才销，失败保留重试）
   │
   ▼ 同帧 draw_frame
DrawList（frame.rs）：每行按 row.mesh.id() **现查** pool/image_cache
   → 拿到新槽位引脚 → push + draw → 画面出现新内容
```

**判定线一句话**：失效与重传都按 `AssetId` 记账，实体账本一行不动（行存柄不存槽），绘制侧每帧现查——所以 N 个引用者共享一份重传，且过渡帧照画旧内容（无闪白）。

## 1. 新文件各管一层

| 文件 | 层 | 职责 |
|---|---|---|
| `remote_assets.rs` | 协议薄壳 | BRP `ash_renderer/mutate_asset` 收 JSON → 调 `mutate_asset`；错误码翻译 |
| `scene/mechanism/asset_ops.rs` | 写入封装 | 四驱动（材质参数/贴图槽/mesh 顶点/image 像素）落成 `Assets::get_mut`——**get_mut 即触发 Modified，写入方零配合** |
| `scene/mechanism/asset_events.rs` | 读侧发现 | 收 `AssetEvent` → 失效凭据 + 关联行重挂脏标 + 事件计量；`Removed/Unused` 只计数不动作 |
| `ledger.rs`（扩展） | 实体账本 | 加 `mark_dirty` 第二发现入口与 `asset_events` 计量字段，行结构不动 |
| `upload.rs`（扩展） | 写侧编排 | 盘点判定放宽"查无**或失效**才传"；按凭据分流 commit/replace；停尸房排空；凭据销账 |
| `pool.rs`（扩展） | mesh 驻留 | `MeshPool::replace` 换槽——旧区间孤儿化（不写旧区间，在飞读天然安全；空间归 4.4） |
| `images.rs`（扩展） | image 驻留 | `ImageCache::take` 旧图移停尸房 + `drain_graveyard` 等帧侧等待后销毁 |
| `descriptors.rs`（扩展） | 描述符表 | `BindlessTables::rebind` 原槽重写——UAB 在飞可写，纹理槽占用不增长 |

一层一句话：**remote_assets 翻译协议、asset_ops 写容器、asset_events 发现、upload 编排、pool/images/descriptors 执行 GPU 侧动作**。依赖单向：协议层 → 场景层 → vulkan 层。

## 2. 逐时刻走一遍（以改 mesh 内容为例）

1. **帧 N·RemoteLast**：`mutate_asset` 拿实体柄 → `Assets::get_mut` 改顶点 → 容器把 `Modified{id}` 排进事件队列。此时 GPU 上的旧区间还在被本帧 draw 读取，什么都不动。
2. **帧 N+1·First**：Messages 双缓冲翻转，事件可读。
3. **帧 N+1·PostUpdate**（`propagate_asset_changes`，账本对账后）：`Modified` 身份进失效集；重扫账本行——引用它的行 `mark_dirty`（本例 2 行：原件+克隆）。计量 +1。
4. **帧 N+1·Last（flush_uploads）**：脏行盘点发现该 mesh"已驻留但持失效凭据"→ 进重传批；转换新内容 → `ensure_capacity`（必要时迁移）→ **分配全新区间**上传 → 提交成功拿票据 → `pool.replace` 原子换槽（条目从旧槽指向新区间，返回旧槽就地孤儿化）→ 凭据销账。全程旧区间**只读不写**，在飞 draw 零风险，驻留不留空洞。
5. **帧 N+1·Last（draw_frame）**：DrawList 组装时每行现查 `pool.resident(id)` → 拿到新槽 → 上屏新内容。**改内容与看到内容同帧完成**。

image 的差别在第 4 步后半：旧 `GpuImage` 不能随条目覆盖而 Drop（在飞 draw 还在采样它）→ 移入停尸房；新图发布走 `rebind` 重写**同一个纹理槽**（描述符指向新 view，槽位占用不变）；下一帧 flush 开头等全部帧槽 fence（4.1.1 闸门）后才销毁停尸房里的旧图。在飞 draw 竞态读描述符，读到的要么是旧图要么是新图，两个本体都活着——无悬空。

材质是"假驻留"：参数每帧从 `Assets<StandardMaterial>` 直读进 UBO，改参数天然生效零上传；`Modified(material)` 只触发引用行重挂脏标，让 flush 重新盘点它的五槽（贴图槽换指时新贴图才可能触发上传）。

## 3. 失败与稳态

- **资产未到货/转换被拒**：重传批跳过该身份，凭据**保留**、行保持脏标下帧重试——凭据不丢，重传不丢；成功才销账。
- **稳态**：零事件 → 读侧早退，不付重扫成本；零脏行 → 空批早退，零上传。停尸房空 → 排空零成本。
- **无引用的失效资产**：失效集条目保留（无人消费它），重传不发生——微小且无害，随凭据生命周期自然收口。

> 对照相邻通道：**换柄**（4.1）动实体行不动资产内容；**改内容**（本流程）动资产条目不动实体行。两条通道并轨进同一份脏行队列。
