# Entity 与 Asset：两套带世代的 ID 体系

> 2026-09-14 建。承接《BSN场景语法与Unity场景对比.md》（篇中 Entity/Handle 已频繁出场但未展开）。两套体系高度同构——都是 **index + generation 的可回收 ID**——但服务对象相反：Entity 管场景内身份，Asset 管大块共享数据。对渲染项目的意义：M2 取数链路和 bindless 池 key 设计直接踩在这两套 ID 上。

## 0. 一句话定位

- **Entity**：ECS 世界里"某行数据"的 8 字节地址，Copy 随处传，不装任何数据；
- **Asset**：大块共享数据（mesh/纹理/材质/声音），**不是实体**，住在按类型分表的资产库里，靠 `Handle`/`AssetId` 引用。

两边的 ID 都用"世代"防悬空复用——和图形程序员熟悉的"槽位+版本号"资源池是同一模式。

## 1. Entity：纯身份，无数据

### 1.1 位布局（0.19.1 已核实）

`crates/bevy_ecs/src/entity/mod.rs:424-432`：

```rust
#[repr(C, align(8))]
pub struct Entity {
    index: EntityIndex,      // NonMaxU32（u32::MAX 留作 PLACEHOLDER 哨兵）
    generation: EntityGeneration,  // u32 世代
}
// repr(C) 字段顺序刻意排成 little-endian 下与 u64 等价
```

对比是走 `to_bits()` 比较（`mod.rs:436-444`，注释明说为了 LLVM 优化 codegen）——**Entity 就是一个 u64**，`Copy`、`Hash`、`Ord` 全部免费。这份"8 字节可拷贝身份"的成本模型对渲染代码很关键：Query 里揣着 Entity 不费任何事，组件字段里存 Entity 引用别的东西也是普通 8 字节。

### 1.2 生命周期与世代防悬空

`mod.rs:74` 注释给出五态模型：分配（alive）→ despawn → 归还给分配器（freed/invalid）→ **generation 自增** → 未来复用同一 index。旧引用拿着"index+旧世代"去 `world.get_entity()` 得 `None` 而非别人的数据——ABA 问题在类型层面根治。

### 1.3 Entity 是万物的统一身份

《World与Resource.md》已核实：Resource = 隐形实体上的组件。0.19 里 **Observer 也是实体**（挂 `Observer` 组件）。所以 World 里所有东西——游戏对象、资源、观察者——都是实体，查询、关系（ChildOf）、事件目标全部用同一个 8 字节 ID 寻址。

### 1.4 与 Unity 对照

| | Unity GameObject | Bevy Entity |
|---|---|---|
| 本质 | 完整 C# 对象（原生侧另有平行结构），带 transform/name 等内建成员 | 纯 ID，一切数据在组件 |
| 传递成本 | 引用（GC 对象，native/managed 双份） | 8 字节 Copy |
| 悬空 | `== null`（Unity 假 null 魔法） | `world.get_entity()` 返回 `None`，类型层面安全 |
| 身份稳定 | 不回收实例 ID | index 回收 + generation 隔离 |

## 2. Asset：大块共享数据的另一套存储

### 2.1 为什么不塞进 ECS

组件存储（archetype 行式表）为**海量小组件的缓存友好遍历**优化；资产是**少量大块、高度共享、需要异步加载/热重载/跨运行稳定引用**的数据。硬塞进实体会让 archetype 装满巨型 blob 且无法表达"多处引用同一份"。所以资产走独立一套：

- **`Assets<A>`**（`assets.rs:288-296`）：本身是 Resource（挂在隐形实体上），内部双存储——`dense_storage`（世代槽位，主路径）+ `hash_map<Uuid, A>`（手动注册的稳定 id）+ 事件队列；
- **每个 asset 是 map 里的值，不是实体**——资产表不知道任何实体的存在；
- **`Handle<A>`**（`handle.rs:134-141`）：`Strong(Arc<StrongHandle>)`（引用计数保活 + index/generation/path 元数据）或 `Uuid(Uuid)`（跨运行稳定常量 id，**不保活**）；
- **`AssetId<A>`**（`id.rs:23-47`）：默认 `Index { index: AssetIndex }`——`AssetIndex { generation: u32, index: u32 }`（assets.rs:23-26，to_bits 拼成 u64）| 手动注册时 `Uuid`。文档原话："cheap to Copy, **can point to an Asset that no longer exists**"（id.rs:19-20）——可悬空是设计特征，配 `Assets::get` 返回 `Option`。

### 2.2 生命周期与异步

路径字符串 → `AssetServer::load` 立即返回 Handle（此时数据未到）→ 后台加载器解析 → 依赖齐全后 `LoadedWithDependencies` 事件 → 入 `Assets` 表。强句柄计数归零才卸载。这正对应《BSN场景语法》里 immediate/queued 两条 spawn 路的分野。

### 2.3 与 Unity 对照

| | Unity | Bevy |
|---|---|---|
| Asset | Project 面板资产（导入产物） | `Assets<A>` 表里的值 |
| 引用 | 序列化 GUID 引用 → 运行时 instanceID | `Handle<A>`（Strong=强引用保活；Uuid≈Addressables 弱引用） |
| 加载 | AssetDatabase + Importer 管线 | AssetServer + AssetLoader，异步 |
| 卸载 | Resources/Addressables 显式管 | 强句柄计数归零自动 |

## 3. 同构性：一套模式，两个领域

| | Entity | AssetId |
|---|---|---|
| 结构 | index(u32) + generation(u32) | AssetIndex{ generation + index }（或 Uuid） |
| 分配器 | `Entities` 回收 index，世代自增 | `AssetIndexAllocator` 回收 index，世代自增（assets.rs:46+） |
| 存储归属 | 组件在 archetype 表，身份在 `World.entities` | 值在 `Assets<A>` 密集槽位 |
| 悬空语义 | get 返回 None | get 返回 None |
| 保活机制 | 无需（数据随实体生死） | 强 Handle 引用计数 |

**对本项目的落点**（对应 M2/M3）：

1. **bindless 池 key 直接用 `AssetId<A>`**：Copy+Hash+Eq，天然就是"槽位+世代"句柄；渲染侧建 `AssetId→GPU槽位` 映射即可，无需自造 id。
2. **Handle 强弱 = 流送驻留策略**：常驻池持有强句柄保活；流出的资源降级为弱引用/仅 AssetId，等计数归零卸载——官方资产生命周期就是现成的驻留换出机制。
3. **M2 取数链路**：`Query<(&Mesh3d, &Transform)>` 拿 `Handle<Mesh>` → 按 id 查 `Assets<Mesh>` 提顶点/索引 → 上传 GPU 常驻池。实体侧与资产侧在此交汇：**组件字段里的 Handle 是 ECS 世界指向资产世界的单向桥**。

## 4. 快速判定：新数据放组件还是资产？

- 每个实例不同、小、跟随实体生死 → 组件（Transform、PointLight）；
- 多处共享同一份、大、独立生命周期、可能来自磁盘 → 资产（Mesh、Image、StandardMaterial）；
- 灰区（比如每个实体一份的小纹理）：默认组件+资产句柄组合，别为省一次间接把大块数据拷进组件。

## 概念盘点

Entity（index+generation u64，纯身份）/ EntityIndex 的 NonMaxU32 哨兵 / generation 防悬空 / 隐形实体（Resource/Observer）/ Assets\<A\>（Resource，双存储）/ Handle::Strong（Arc 引用计数保活）/ Handle::Uuid（跨运行稳定不保活）/ AssetId::Index vs AssetId::Uuid / AssetIndexAllocator 回收 / AssetServer 异步加载 / 单向桥（实体→资产）。
