# 第5a站：bindless 数据怎么进显存

本篇讲第 5 站上半：网格、贴图和对象记录怎样从 CPU 进到 GPU 显存，以及换下来的旧数据什么时候才能回收。shader 怎么按下标找到这些数据见《[第5b站：shader 怎么找到它们](第5b站：shader%20怎么找到它们.md)》。

> 进度：讲解正本已写好（2026-10-30），待用户逐段阅读校验。依据 = 2026-10-30 工作区源码（main=51884896a 加未提交的 `memory/` 目录拆分、`descriptors.rs` → `bindless_texture_table.rs` 改名、`slot_allocator.rs` 与 `frame_uniform_table.rs` 拆出）。代码引用写符号名不写行号。

## 总图：一份数据进显存的四步

```text
Bevy 资产（Mesh / Image）
   │  mesh_convert / image_spec：CPU 侧整形，零 Vulkan 调用
   ▼
staging（HOST_VISIBLE，宿主 memcpy，非 coherent 时自动 flush）
   │  Uploader::submit_batch：transfer 队列一次提交，signal 时间线票据 N
   ▼
DEVICE_LOCAL 目的地：MeshPool 顶点/索引池 · GpuImage · 对象表
   │  驻留登记（MeshPool::commit / ImageCache::swap_commit），票据随行
   ▼
图形提交在 ALL_COMMANDS 阶段等票据 → shader 读到的是拷完的数据
```

本站只有一条主线：**写入走 staging + 票据，替换走“新位置 + 旧位置进停尸房”，回收要等两个条件同时成立**。下面按 `core/device/memory/` → `uploader` → `pool` → `images` 的读序展开。

## 一、内存契约：所有 buffer 只有一种建法

`core/device/memory/` 三个文件各管一件事，全库的表、池、环都只引用它们，不另立规则。

- **`role.rs` 的 `BufferRole::spec`**：每种用途要什么 usage、内存类型“必须有”和“最好有”哪些属性，只写这一张表。
- **`contract.rs` 的 `MemoryContract`**：启动时查一次设备的内存类型表、堆表和 `nonCoherentAtomSize` 并冻结。`find_type` 在 `memoryTypeBits` 允许的类型里挑：必需属性必须全含，优先属性命中越多越好，同分取小索引。**必需属性给不出就报错，不悄悄降级**。
- **`buffer.rs` 的 `GpuBuffer`**：按 `requirements.size` 恰量分配一块内存、偏移 0 绑定；宿主可见的从创建到销毁一直映射着。`write` 在非 coherent 内存上自动按 atom 对齐 flush，`read` 前自动 invalidate。

| 角色 | usage 要点 | 必需属性 | 优先属性 | 本项目用在 |
|---|---|---|---|---|
| `DevicePool` | 顶点＋索引＋传输两向 | DEVICE_LOCAL | — | MeshPool 两条池 |
| `Staging` | TRANSFER_SRC | HOST_VISIBLE | HOST_COHERENT | 上传暂存 |
| `Readback` | TRANSFER_DST | HOST_VISIBLE | CACHED＋COHERENT | 诊断回读 |
| `FrameUniform` | UNIFORM | HOST_VISIBLE | CACHED＋COHERENT | set1 每帧 UBO |
| `Storage` | STORAGE＋传输两向 | HOST_VISIBLE | CACHED＋COHERENT | 模型/材质环、光源表等帧槽环 |
| `DynamicDraw` | 顶点＋索引 | HOST_VISIBLE | CACHED＋COHERENT | UI 顶点环 |
| `DeviceStorage` | STORAGE＋传输两向 | DEVICE_LOCAL | — | 对象表、cluster 列表 |
| `IndirectCommands` | STORAGE＋INDIRECT | DEVICE_LOCAL | — | indirect 命令环 |

**`GpuBuffer` 不防 GPU 在飞读写**。映射指针随时能写，“等 GPU 用完再写、再销毁”是上层用票据和帧槽 fence 保证的；它的 `Drop` 也不等 GPU（退出时由 `teardown_vulkan` 先 `device_wait_idle`）。

跨队列族共享：`GpuBuffer::create_with_families` 收到两个不同队列族时按 CONCURRENT 创建。本机有专用 transfer 族，池和对象表都走 CONCURRENT——它们被 transfer 反复写、被图形长期读，每批都做一次所有权转移不划算；写到读的可见性改由票据等待保证（见第二节）。

## 二、上传器：staging 环 + 时间线票据

`core/device/transfer.rs` 的 `Uploader` 把一批拷贝合成**一次 transfer 提交**，完成后用时间线信号量的单调值（`Ticket`，u64）表达“这批数据什么时候能用”。

**构成**：2 个轮转槽（init 时定，每槽初始 1 MiB staging，按需整槽扩容），每槽 = 一块 staging + 一条 transfer 命令缓冲 + 一条 graphics 命令缓冲（mip 续批用）+ 本槽最后一张票据。

**`submit_batch` 一次走完**：

1. 空批（无字节、无拷贝）直接返回 `None`，不发票据。
2. 取下一个槽；若这个槽上一批的票据还没完成，**CPU 在这里等**并打日志。这是 staging 复用的唯一等待点。
3. staging 不够大就整块换新（旧块的 transfer 读已被上一步的等待证明完成）。
4. `GpuBuffer::write` 把整批字节写进 staging（非 coherent 时自动 flush）。
5. 录制：buffer 拷贝 → 图像段（`UNDEFINED → TRANSFER_DST` → 拷 level 0 → 单级图迁到 `SHADER_READ_ONLY`，多级图的 level 0 迁到 `TRANSFER_SRC` 留给续批）→ 池迁移用的设备侧拷贝及其写后读屏障 → 队列族所有权 release（生产路径为空，见下）。
6. 提交到 transfer 队列，signal 时间线值 = 新票据，fence 传空。
7. **提交成功之后**才记下本槽票据并让 `next_ticket` 加一。上面任何一步失败都提前返回，所以不存在“发了票据却没提交”的情况。

**多级贴图的续批**：blit 必须在 graphics 族的命令池里录（transfer-only 池录 blit 违规）。所以有多级图时，上传器再向 graphics 队列提交一段续批：等 transfer 票据 N → 逐级 blit 生成 mip 链 → 整链迁到 `SHADER_READ_ONLY` → signal N+1。**发布给调用方的票据是 N+1**，消费方的等待写法不变。

**票据有两类等待方**：

| 等待方 | 怎么等 | 用在哪 |
|---|---|---|
| 图形提交（GPU 侧） | `queue_submit` 的 wait 列表挂票据信号量，值 = `last_issued_ticket()`，阶段 `ALL_COMMANDS` | 每帧提交（`record_frame` 末尾）。等的是“截至目前发出的全部上传”，不是某一份资源自己的票据 |
| `Uploader::wait_until`（CPU 侧） | `vkWaitSemaphores` | staging 槽复用、池扩容迁移、停尸房排空、建表时的 fallback 白图 |

图形侧那次等待同时承担**内存可见性**：资源是 CONCURRENT 共享的，没有所有权转移屏障，transfer 写对图形读可见全靠这条信号量依赖。

**队列族所有权**：`Release`/`ImageRelease` 是“所有权从 transfer 族让渡给 graphics 族”的屏障描述，**不是 staging 回收**。生产路径上图像和池都按 CONCURRENT 建，所以这两个列表恒为空；release/acquire 的录制代码保留，由探针跨族组实证。

## 三、mesh 大池：两条 DEVICE_LOCAL 池 + 按资产去重

`resident/pool.rs` 的 `MeshPool` 持有一条顶点池、一条索引池，全部 mesh 都住在里面；`resident/mesh_convert.rs` 负责把 Bevy `Mesh` 整形成池能直接收的字节。

**转换产物**（`convert_mesh` → `ConvertedMesh`）：交错顶点 32 B（POSITION 12 + NORMAL 12 + UV_0 8），索引一律 U32，同时扫出局部包围盒。只收 TriangleList；POSITION 缺失或格式不对就拒收（Tier①，warn 一次跳过），NORMAL/UV 缺失补 0 继续。

**分配**：

- 懒建：第一次 `ensure_capacity` 才建池，容量按当时收齐的需求算。
- `alloc` 先在 free list 里 first-fit 找空洞（整段用或切头留尾），找不到才推进 bump 游标。顶点区间按 32 B、索引区间按 4 B 对齐，碎片不合并。
- `MeshSlot` 记两段区间、顶点/索引数、包围盒和**发布它的上传票据**；绘制用的两个引脚由区间换算：`vertex_base = 顶点字节偏移 ÷ 32`，`first_index = 索引字节偏移 ÷ 4`。
- `residency: HashMap<AssetId<Mesh>, MeshSlot>` 按资产身份去重，已驻留的不会因为每帧盘点重复上传。

**扩容迁移**（`ensure_capacity` 放不下时）：新容量 = max(需量 × 4/3, 现容量 × 2, 1 MiB) 再对齐 1 MiB。顺序是

```text
等全部在飞上传完成 → 建两条新池 → 设备侧整段拷贝 [0, used) → 等迁移票据
→ wait_draws（等全部帧槽 fence，没有在飞 draw 还在读旧池）→ 换上新池、旧池销毁
```

这是一次显式停顿，记在 `MaintenanceLedger` 里，与正常上传分开计账。数据按原偏移平移，已发出的 `vertex_base`/`first_index` 不变。

**替换与卸载**：`replace`（同身份换内容）和 `retire`（卸载）都把旧区间连同旧票据推进停尸房，**不立即复用**——在飞 draw 可能还在读。`drain_graveyard` 先 `wait_draws`，再按停尸房里的最大票据 `wait_until` 一次，然后把区间还给 free list。

## 四、贴图缓存：一张图一块 dedicated memory

`resident/images.rs` 分三层：`ImageSpec`（规格映射）→ `GpuImage`（Vulkan 对象本体）→ `ImageCache`（按资产身份驻留）。

**`image_spec`**：只收 2D、`Rgba8Unorm`/`Rgba8UnormSrgb` 且有像素数据的 Bevy `Image`，其余拒收（Tier①）。sRGB 与否沿用 glTF 加载器定好的格式，本层不再推断；`ImageSampler::Default` 解析为 `ImageSamplerDescriptor::linear()`（与 Bevy `ImagePlugin` 默认一致）。mip 级数按全链 `floor(log2(max(w,h))) + 1` 算。

**`GpuImage::create`**：

- OPTIMAL tiling，usage = TRANSFER_DST + TRANSFER_SRC + SAMPLED，建前核对格式的 optimal tiling feature。
- 格式缺 blit 或线性过滤支持就降为单级，有日志。
- 每张图一块 dedicated `VkDeviceMemory`（必需 DEVICE_LOCAL），view 覆盖全部 mip 级。
- **采样器不在这里建**：同参数的贴图共用一个采样器槽，采样器本体归 `BindlessTextureTable` 持有（第5b站）。
- 跨队列族时按 CONCURRENT 建。原因不只是省所有权转移：EXCLUSIVE 加 release/acquire 时，验证层对“已写入但还没被 draw 访问的 UAB 数组元素”的布局账不随 release 更新，会误报 Draw-09600。

**`ImageCache`**：`resident: HashMap<AssetId<Image>, (GpuImage, Ticket, SlotBinding)>`，第三项是这张图在 bindless 表里的两个下标。

- `swap_commit`：新图入账；同身份已有旧图时，旧图带着旧票据进停尸房，旧槽位交回 `BindlessTextureTable::retire`。**必须在 `BindlessTextureTable::publish` 之后调用**——先发布新槽再交回旧槽，同参数采样器的引用数才不会中途归零。
- `retire`：卸载，同样是旧图进停尸房、槽位交回。
- `drain_graveyard`：与网格停尸房同一套双条件，`wait_draws` 加 `wait_until(最大票据)`，之后才真正销毁旧图。

## 五、一轮上传怎么串起来

编排在 `driver/upload.rs` 的 `flush_uploads`（`Last` 调度，在 `draw_frame` 之前），细读归第 9 站，这里只看顺序：

1. **先排空停尸房**：贴图、网格区间、bindless 待回收槽、对象表旧实例槽，四条路径各自判空，非空才调 `frames.wait_all_inflight()`。
2. 消费卸载事件：驻留条目出账进停尸房，下一轮第 1 步再排空。
3. 盘点账本脏行，收集要传的 mesh、贴图和要写的对象记录（48 B，按新分配的实例槽定址）。
4. 资产转换与规格映射，未到货的下帧重试。
5. `ensure_capacity` → 建 `GpuImage` → 在一个 `Vec<u8>` 里连续排好网格两段、每张贴图 level 0、每条对象记录。
6. `submit_batch` 一次提交，拿到票据。
7. **提交成功后**才登记驻留：网格 `commit`/`replace`；贴图逐张 `publish` 拿槽位，再 `swap_commit`。

“提交成功才登记”保证了失败路径不会留下指向未上传数据的账本行。

## 六、三种“完成”别混用

| 要证明的事 | 证据 | 不能替代 |
|---|---|---|
| CPU 写的字节对设备可见 | 非 coherent 内存 flush（`GpuBuffer::write` 内建） | 不能证明 GPU 已读完旧内容 |
| 上传拷贝已完成 | 时间线票据（GPU 侧挂 wait，CPU 侧 `wait_until`） | 不能证明图形 draw 已不再读旧位置 |
| 图形侧最后一次使用已结束 | 全部帧槽 fence（`wait_all_inflight`） | 不能证明上传完成：窗口最小化时上传仍会提交，但没有图形提交 |

所以回收旧区间和旧图要等**票据和帧 fence 两个条件都满足**，缺一不可。机制背景见《[CPU 与 GPU 异步访问：三种存储与更新策略](CPU与GPU异步访问：三种存储与更新策略.md)》。

## 校验点

- **staging 复用靠什么证明安全？** 每槽记最后一张票据，复用前查时间线计数，未完成就 CPU 等待——对照第 3 站：这里等的是票据，不是帧 fence。
- **图像段为什么和 buffer 拷贝分开录？** 图像屏障携带 old/new layout 和子资源范围，要解决布局迁移；buffer 只有读写次序问题。单级图和多级图的收尾布局也不同（对照第 4 站屏障语义）。
- **必需属性不满足会怎样？** `find_type` 返回 `VulkanError::Init`，带上需要什么、优先什么和设备允许位，不静默降级。
- **图形提交等的是哪张票据？** `last_issued_ticket()`，也就是截至组装 `FrameDraw` 时发出的最新一张。资源各自的票据只用在停尸房排空和迁移上。
- **建表时的 fallback 白图为什么当场等完？** `GpuImage` 的 `Drop` 不等 GPU；如果建表后面任一步失败把它丢掉，拷贝可能还在写。

## 相关源码

- `ash_renderer/src/render/core/device/memory/`：`role.rs`、`contract.rs`、`buffer.rs`。
- `ash_renderer/src/render/core/device/transfer.rs`：`Uploader`、上传批次类型与 `Ticket`；`ash_renderer/src/render/resident/`：`pool.rs`、`mesh_convert.rs`、`images.rs`。
- `ash_renderer/src/driver/upload.rs`：`flush_uploads` 编排（第 9 站细读）；`ash_renderer/src/render/frame/frame_draw.rs`：图形提交挂票据等待。
