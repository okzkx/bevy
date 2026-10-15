# 对象表机制：GPU可见的场景名册——记录布局、set2与槽位生命周期

> 2026-10-15 收官配套机制篇（机制深潜之二）。本文是对象表的完整机制档案：一个记录里每个字段为什么在那、48B 怎么排出来、描述符怎么挂、槽位从生到死的每一步谁负责。为什么三张表这么分（设计取舍）见[设计决策篇](三张表的设计决策：更新频率、写入方式与备选方案对照.md)；运行时序走读见[流程导读](GPU常驻绘制数据流程：从账本行到实例索引的各级账面.md)。

## §0 判定线（本模块的不变式，全部机制为它服务）

**对象表的每一条在用记录，任何时刻被任何 shader 读取，内容都是完整的且属于当前占用者**——"写一次"是这张表的全部：字段只描述"物体是什么"（定义），不描述"物体现在怎样"（状态，归模型环）。所有机制（staging 写入、换槽、停尸房）都是这条不变式的代价。

## §1 记录布局：48B 的每个字节都有名字

```wgsl
// debug_draw.wgsl——GPU 视角
struct ObjectRecord {
    first_index: u32,      // 索引池：这段 mesh 的索引从哪个号开始
    index_count: u32,      // 画多少个索引
    vertex_base: u32,      // 顶点池：首顶点号（vertexOffset）
    material_index: u32,   // 材质环下标（解引用链的第二跳）
    aabb_min: vec4f,       // 局部包围盒（w 垫 0）
    aabb_max: vec4f,       // 同上
}   // 6×8B = 48B，vec4 对齐 16 → 数组 stride 48 ✓
```

逐字段的设计依据：

- **范围三参数（12B）**：5.1 阶段 CPU 侧绘制引脚仍从 `MeshSlot` 现查（`cmd_draw_indexed` 的参数还是 CPU 给的），表里存副本是**给 5.3 indirect 备粮**——届时 compute 从记录里读这三个数写进 `VkDrawIndexedIndirectCommand`，CPU 侧引脚退役。副本与引脚并存是过渡态，两处数据同源（都出自 MeshSlot），不会分叉（换绑时一起换）。
- **material_index（4B）**：对象→材质的解引用跳。指向材质环的**槽号**（跨帧稳定），不是内容——内容变化由材质环每帧重投影吸收（设计决策篇 §4），这是"材质内容变化不需要碰对象表"的结构保证。
- **局部包围盒（32B）**：**存局部不存世界**是关键决策。世界包围盒随 model 每帧变，存进"写一次"的表就会失真——存局部（几何固有属性，转换时一遍扫出）保住写一次不变式；世界空间推导（8 角点保守变换）是纯函数，5.2 在 GPU 侧用 model 现场变换。w 分量垫 0（vec4 对齐）。
- **为什么没有 model**：它是状态不是定义，原地更新撞 hazard、换槽撞高频停顿——拆到模型环（设计决策篇 §3 有完整论证链）。

**布局契约的钉法**：WGSL 结构与 Rust 镜像 `ObjectRecordLayout` 用 `offset_of!` 静态断言互证（first_index@0 / material_index@12 / aabb_min@16 / aabb_max@32 / size=48），`pack_object_record` 按偏移手工排布小端字节——WGSL/描述符 range/CPU 三方被编译期钉死。这是本仓库从 push 时代延续的纪律："repr(C) 或低于 128B 都不能独立证明对齐"，断言在编译期执法。

## §2 描述符面：set2 怎么挂进管线

```
set2（自建 pool，每帧槽一套，init 一次写入永不更新）
  b0  STORAGE_BUFFER → 对象表 buffer（全量 range）
  b1  STORAGE_BUFFER → 模型环[本帧槽]（全量 range）
  b2  STORAGE_BUFFER → 材质环[本帧槽]（全量 range）
```

- **只读 storage**（`var<storage, read>`）：shader 只读，naga 产出 NonWritable 修饰的 SSBO——读侧语义自洽（表内容归写者管的纪律在类型上可见）。
- **描述符 init 一次写入**：buffer 容量固定不重建 → 描述符指向的 buffer 永不换 → 没有任何运行期描述符更新。这和 set0 的 UAB（运行期逐槽发布）是两种更新语义，所以**自建 pool** 不混进 BindlessTables 的 UAB 池（pool 旗标、容量对账、max_sets 各管各的）。
- **stage 超集** `COMPUTE|VERTEX|FRAGMENT`：5.2 的 compute 剔除将读同一批表——layout 一次声明到位，届时零改动。
- b1/b2 按**帧槽各一套 set**：环按帧槽轮转，描述符必须跟着指到"本帧那一份"——这是环结构对描述符形状的直接要求。
- 与 set0/set1 的关系：三个 set = 三种变化节奏（资产级/每帧全局/per-object），保留旧集是因为里面的数据照旧在用；set2 不并入 set0 是冻结边界解冻最小化（旧集 binding 表与 UBO 128B 一个字节没动）。

## §3 内存面：BufferRole::DeviceStorage

新角色 = `STORAGE_BUFFER | TRANSFER_DST | TRANSFER_SRC` + **DEVICE_LOCAL 硬契约**（不给 host-visible 降级路径——常驻的意义就是 GPU 独享显存）：

- **TRANSFER_DST**：flush 的 staging 批次往里拷（记录 48B/条，`dst_offset = slot × 48` 定址）。
- **TRANSFER_SRC**：将来容量重建/回读校验的拷贝源（与顶点池同款前瞻）。
- **CONCURRENT 双族**（有专用 transfer 族时）：transfer 写 + graphics 长期读，所有权乒乓不值得——与顶点/索引池同一决策；跨队列内存可见性由图形提交等票据信号量收口（同一执法点，无新机制）。
- 容量固定 131072 槽（覆盖 10 万档，6MB）：**用尽报错出局不静默截断**（TABLE_CAPACITY 同纪律）。为什么不学 MeshPool 做容量迁移？记录 48B 极小，10 万档才 6MB——上界远大于可见需求，迁移机制（等待+整段拷贝+偏移不变性论证）的复杂度买不回；5.4 真撞上界再评估（设计决策篇 §5 翻面条件）。

## §4 槽位生命周期：生、死、复用

```
生：alloc_object_slot() = free list 优先（first-fit）→ bump 游标兜底
    槽号写进账本行 instance_slot；记录 48B 随本批 staging 进表（票据保护）
死：despawn/换绑 → 旧槽进 ledger.freed_slots（纯整数交接账）
    → flush 开头送 InstanceTables 停尸房（不可复用态）
复用：下帧 flush 开头 drain_graveyard：
    等"图形最后使用"（全部帧槽 fence = 无任何在飞 draw）→ 归还 free list
```

三个值得钉住的点：

1. **为什么交接账要过一层 `freed_slots`（账本直接调 InstanceTables 不行吗）**：账本（scene 层）零 Vulkan 是 4.1 立的边界——账本只该知道"这个槽号不再属于我"，不该知道停尸房是什么。整数交接账让两层保持单向依赖，flush（编排层）是唯一同时看得见两边的角色。
2. **为什么停尸房只需要帧侧等待、不需要票据等待**（MeshPool 的停尸房是双条件）：旧记录的写入票据早已完成（它是很久以前提交的 transfer）；复用的危险只有"新写入 vs 在飞旧读"，帧侧等待独木可撑。pool 那边带票据是历史形状（覆盖上传与回收共用节拍），不是本表的必需。
3. **used_objects 是历史峰值语义**（复用不推进游标）：与 MeshPool 的 used 游标同款——判定线"反复增删存量回基线"的读数口，`object_slots`（峰值）与 `graveyard`（在途）分列，不混报"当前真占用"。

**材质槽例外**：bump-only 永不回收（`ensure_material_slot` 查无才分配，跨帧稳定）。理由：量级 = 唯一材质数（小），回收需要"还有没有行引用"的引用计数（贵），泄漏有界且上界显式（65536，超限报错）——工程判断，不是遗漏。对象记录里的 `material_index` 因此可以承诺跨帧稳定，这是"材质内容变化不需要碰对象表"的另一半保证。

## §5 数据从哪来：一条单向的供给链

```
convert_mesh（纯 CPU）——顶点交错同一遍扫出局部 AABB
  → ConvertedMesh.local_aabb
  → pool.commit/replace 落进 MeshSlot.local_aabb（资产层缓存）
  → flush 行扫描：MeshSlot（范围+票据+AABB）+ 材质槽号
  → pack_object_record（手工排布 48B 小端）
  → staging → 对象表[slot]
```

局部 AABB 放 `MeshSlot`（资产层）而不是账本行：它属于"mesh 内容"不属于"实例"——同一 mesh 的 N 个实例共享一份 AABB，资产层缓存天然去重。**存局部不存世界**：世界 AABB 随 model 变，存进"写一次"的表必失真；世界推导是纯函数（`conservative_world_aabb`，8 角点保守收紧 + 单测四枚），5.2 GPU 侧现场变换。

记录身份三元组 `bound = (mesh_id, mesh_ticket, material_id)` 挂账本行做免重写判定：三者不变 = 记录内容不变（免换槽免上传）。**含 ticket** 是防"同柄内容重传"漏检——同 AssetId 的 mesh 内容被 Modified 后，身份没变但范围/包围盒全变了，ticket（新上传批次号）必换，身份比对靠它失效。

## §6 消费端契约（shader 视角）

```wgsl
let obj = objects[push.instance];   // 下标 = push 常量，同 draw 内 uniform
```

- 下标是 push（draw 内恒定）→ 不需要 NonUniform 修饰（那是 draw 内逐线程下标不同才需要的）。
- 消费侧唯一假设 = §0 不变式：任何在用槽读出来都是完整记录。shader 不检查、不重试、不回退——不变式由写侧全部机制担保，读侧零成本。
- 5.2/5.3 的接入面已预留：stage 超集含 COMPUTE；记录里的范围三参数就是 indirect 写参的原料。
