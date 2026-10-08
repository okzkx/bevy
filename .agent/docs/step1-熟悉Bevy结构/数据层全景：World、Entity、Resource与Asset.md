# 数据层全景：World、Entity、Resource 与 Asset

> 2026-09-14 建两篇（《World与Resource》《Entity与Asset》），2026-09-16 合并重构去重。数据层四件套一篇讲完：**World 总仓库 → Entity 纯身份 → Resource 类型即键的单例 → Asset 另一套存储**。重复消解原则：`Resource=隐形实体` 只在 §3 说一次；"数据放哪"判定合并成 §7 一张表。对渲染项目的意义：M2 取数链路和 bindless 池 key 直接踩在 Entity/AssetId 两套 ID 上。

## 1. World：数据总仓库（字段级解剖）

`crates/bevy_ecs/src/world/mod.rs:98-115`，字段按职责分五组：

| 组 | 字段 | 职责 |
|---|---|---|
| 实体 | `entities: Entities`、`entity_allocator` | 实体 id 分配与回收（带世代防复用悬空） |
| 存储表 | `components`、`archetypes`、`storages`、`bundles` | 组件的表结构（原型 = 组件组合，表 = 列式存储）——Query 遍历的数据底座 |
| **资源** | `components` + **`resource_entities`** | 资源内部就是"挂在隐形实体上的组件"，见 §3 |
| 观察者 | `observers`、`removed_components` | Observer 回调注册表、组件移除消息 |
| 变更检测 | `change_tick: AtomicU32`、`last_change_tick`、`last_check_tick` | 全局 tick 就存在 World 里，见 §5 |
| 命令队列 | `command_queue: RawCommandQueue` | Commands 的暂存地，见 §6 |

**三个要点**：

1. **World 是被动的**：它不跑逻辑。"世界在动"= 挂在它上面的 Schedule 在跑。
2. **读写只有三条路**：① 系统参数（常态，调度器管借用安全，目录见《System参数与数据访问》）；② `World` API 直接操作（`world.spawn()` / `world.resource_mut::<T>()`——用于引导代码、测试、一次性脚本，`externally_driven_headless_renderer.rs` 里 `self.0.main.world_mut().spawn(...)` 就是范例）；③ 独占系统（`&mut World` 整借，牺牲并行换自由）。
3. **与 Unity 类比再校准**：World ≈ "所有已加载场景的对象仓库 + 全局单例"合并成的数据容器；Unity 的场景切换在 Bevy 里只是"对 World 批量 spawn/despawn"。

## 2. Entity：纯身份，无数据

### 2.1 位布局（0.19.1 已核实）

`crates/bevy_ecs/src/entity/mod.rs:424-432`：

```rust
#[repr(C, align(8))]
pub struct Entity {
    index: EntityIndex,      // NonMaxU32（u32::MAX 留作 PLACEHOLDER 哨兵）
    generation: EntityGeneration,  // u32 世代
}
// repr(C) 字段顺序刻意排成 little-endian 下与 u64 等价
```

比较走 `to_bits()`（`mod.rs:436-444`，注释明说为了 LLVM 优化 codegen）——**Entity 就是一个 u64**，`Copy`、`Hash`、`Ord` 全部免费。成本模型对渲染代码很关键：Query 里揣着 Entity 不费任何事，组件字段里存 Entity 引用别的东西也是普通 8 字节。

### 2.2 生命周期与世代防悬空

`mod.rs:74` 注释给出五态模型：分配（alive）→ despawn → 归还分配器（freed）→ **generation 自增** → 未来复用同一 index。旧引用拿"index+旧世代"去 `world.get_entity()` 得 `None` 而非别人的数据——ABA 问题在类型层面根治。

### 2.3 Entity 是万物的统一身份

**Resource 是隐形实体上的组件**（§3 详述）；0.19 里 **Observer 也是实体**（挂 `Observer` 组件）。所以 World 里所有东西——游戏对象、资源、观察者——都是实体，查询、关系（ChildOf）、事件目标全部用同一个 8 字节 ID 寻址。

### 2.4 与 Unity 对照

| | Unity GameObject | Bevy Entity |
|---|---|---|
| 本质 | 完整 C# 对象（native 侧另有平行结构），带 transform/name 内建成员 | 纯 ID，一切数据在组件 |
| 传递成本 | 引用（GC 对象，native/managed 双份） | 8 字节 Copy |
| 悬空 | `== null`（Unity 假 null 魔法） | `world.get_entity()` 返回 `None`，类型层面安全 |
| 身份稳定 | 不回收实例 ID | index 回收 + generation 隔离 |

## 3. Resource：类型即键的全局单例

### 3.1 底层就是组件（隐形实体机制）

**0.19.1 关键事实**（`crates/bevy_ecs/src/resource.rs:87`）：

```rust
pub trait Resource: Component {}
```

每个资源内部对应一个**隐形实体**（`resource_entities` 缓存维护 ComponentId → Entity 链接）。这不是冷知识——它统一解释了：资源天然参与变更检测（`Changed<Res<T>>` 有效）、天然能被 Observer 观察、一套存储机制服务两种数据。

### 3.2 为什么每类型只能有一份

**不是存储限制，是 API 键选型**。`Res<T>` / `world.resource::<T>()` 的全部寻址信息就是类型——零运行时查找。这套薄 API 依赖"每类型恰好一份"的前提。ECS 的身份轴因此分成两根：

| | 身份轴 | 语义 | 参数 |
|---|---|---|---|
| Resource | **类型**（编译期） | 全局唯一的服务/仓库 | `Res`/`ResMut` |
| Component | **(实体, 类型)**（运行期） | 一类东西的多个实例 | `Query` |

想要"多份"，要么给 `Res<T>` 加第二把钥匙（所有系统改签名），要么用已有的实体+组件——Bevy 选后者，不做重复机制。内置资源全是"天然单例"：`Time`、`Assets<T>`、`Schedules`（调度器本身！）、`MainScheduleOrder`、`AppTypeRegistry`、`ClearColor`。

**遇到"想要多个"的三条正规出路**：

1. **Newtype 起名字**：`struct PlayerStats(Stats)` / `struct EnemyStats(Stats)`——类型即名字；
2. **一个资源内部装集合**：`Assets<T>` 本尊就是范本——一个资源装百万资产，键是 `AssetId`（bindless 池同款形状）；
3. **升格为实体+组件**：多相机就是范例——单例语义变数据语义，访问从 `Res` 换成 `Query`/`Single`。

**唯一性的另一半：后插覆盖**。`insert_resource` 文档原话 "you will overwrite any existing data"（world/mod.rs:1994）——同名资源插两次不报错，旧值被换掉。坑：两个插件各 insert 一份同类型配置，后者悄悄赢。

### 3.3 用法速查

- 声明 `#[derive(Resource)]`；注入 `app.init_resource::<T>()`（Default）/ `insert_resource`（带值）；
- 参数：`Res<T>`（读）/ `ResMut<T>`（写，打 Changed）/ `Option<Res<T>>`（容忍缺失）；
- **`NonSend<T>`/`NonSendMut<T>`**：非线程安全资源（如 `WinitWindows`），系统被钉在主线程——第二步拿窗口句柄要用；
- **`Local<T>`**：系统级私有状态（每系统实例一份、跨帧保留、不参与冲突判定）——hello_world 计数器、`Main::run_main` 的首帧布尔。详解见《System参数与数据访问》§4。

## 4. Asset：大块共享数据的另一套存储

### 4.1 为什么不塞进 ECS

组件存储（archetype 行式表）为**海量小组件的缓存友好遍历**优化；资产是**少量大块、高度共享、需要异步加载/热重载/跨运行稳定引用**的数据。硬塞进实体会让 archetype 装满巨型 blob 且无法表达"多处引用同一份"。所以资产走独立一套：

- **`Assets<A>`**（`assets.rs:288-296`）：本身是 Resource（挂在隐形实体上），内部双存储——`dense_storage`（世代槽位，主路径）+ `hash_map<Uuid, A>`（手动注册的稳定 id）+ 事件队列；
- **每个 asset 是 map 里的值，不是实体**——资产表不知道任何实体的存在；
- **`Handle<A>`**（`handle.rs:134-141`）：`Strong(Arc<StrongHandle>)`（引用计数保活 + index/generation/path 元数据）或 `Uuid(Uuid)`（跨运行稳定常量 id，**不保活**）；
- **`AssetId<A>`**（`id.rs:23-47`）：默认 `Index { index: AssetIndex }`——`AssetIndex { generation: u32, index: u32 }`（assets.rs:23-26，to_bits 拼成 u64）| 手动注册时 `Uuid`。文档原话："cheap to Copy, **can point to an Asset that no longer exists**"（id.rs:19-20）——可悬空是设计特征，配 `Assets::get` 返回 `Option`。

T 不限官方几个：任何 `#[derive(Asset)]` 的类型（trait 约束 `VisitAssetDependencies + TypePath + Send + Sync + 'static`，bevy_asset/src/lib.rs:452）+ `app.init_asset::<T>()` 注册即可，自定义资产一等公民。

### 4.2 生命周期与异步

路径字符串 → `AssetServer::load` 立即返回 Handle（此时数据未到）→ 后台加载器解析 → 依赖齐全后 `LoadedWithDependencies` 事件 → 入 `Assets` 表。强句柄计数归零才卸载。对应《BSN场景语法》immediate/queued 两条 spawn 路的分野。

### 4.3 与 Unity 对照

| | Unity | Bevy |
|---|---|---|
| Asset | Project 面板资产（导入产物） | `Assets<A>` 表里的值 |
| 引用 | 序列化 GUID 引用 → 运行时 instanceID | `Handle<A>`（Strong=强引用保活；Uuid≈Addressables 弱引用） |
| 加载 | AssetDatabase + Importer 管线 | AssetServer + AssetLoader，异步 |
| 卸载 | Resources/Addressables 显式管 | 强句柄计数归零自动 |

## 5. 变更检测的落点：tick 就在 World 字段里

- `change_tick: AtomicU32` 是全 World 的逻辑时钟；每次**可变访问**（`DerefMut`、`&mut`）把该数据的 tick 打成当前值；
- 系统自带 `last_run`/`this_run` tick 窗口：`Changed<T>` = 数据 tick 落在窗口内；
- 帧末 `main.world.clear_trackers()`（见《SubApp机制与取舍》§2）清窗口——extract 必须赶在它之前，这是 extract 时序契约的底层原因。

## 6. Commands 与延迟生效（入口级）

- `Commands` 参数不立即改 World：操作进 `command_queue`，之后批量应用；
- **同步点三时机**（依赖边自动插 `ApplyDeferred` / 显式排 `ApplyDeferred` / schedule 末尾 final apply）与手动控制全解 → 《System参数与数据访问》§3；
- **对渲染采集的直接含义**：`Commands::spawn` 的实体当帧 Query 未必看得到，要等同步点之后——采集系统别假设"同系统里 spawn 就能查到"。

## 7. 数据放哪：一张判定表（三层）

| 数据特征 | 归宿 | 例 |
|---|---|---|
| 全游戏只有一份 | **Resource** | Time、输入状态、`Assets<T>` |
| 每个实例不同、小、跟随实体生死 | **Component** | Transform、PointLight、"每个敌人的血量" |
| 多处共享同一份、大、独立生命周期、可能来自磁盘 | **Asset**（实体侧只揣 Handle） | Mesh、Image、StandardMaterial |

灰区：每个实体一份的小纹理 → 组件字段存 `Handle<Image>` 组合，别为省一次间接把大块数据拷进组件。**单例长成多例 = 从 Resource 迁去实体+组件**（多相机范式），不是给资源找第二把键。

## 8. 同构性：一套模式，两个领域（Entity vs AssetId）

| | Entity | AssetId |
|---|---|---|
| 结构 | index(u32) + generation(u32) | AssetIndex{ generation + index }（或 Uuid） |
| 分配器 | `Entities` 回收 index，世代自增 | `AssetIndexAllocator` 回收 index，世代自增（assets.rs:46+） |
| 存储归属 | 组件在 archetype 表，身份在 `World.entities` | 值在 `Assets<A>` 密集槽位 |
| 悬空语义 | get 返回 None | get 返回 None |
| 保活机制 | 无需（数据随实体生死） | 强 Handle 引用计数 |

**对本项目的落点**（对应 M2/M3）：

1. **bindless 池 key 直接用 `AssetId<A>`**：Copy+Hash+Eq，天然就是"槽位+世代"句柄；渲染侧建 `AssetId→GPU槽位` 映射即可，无需自造 id。
2. **Handle 强弱 = 流送驻留策略**：常驻池持强句柄保活；流出资源降级为弱引用/仅 AssetId，等计数归零卸载——官方资产生命周期就是现成的驻留换出机制。
3. **M2 取数链路**：`Query<(&Mesh3d, &Transform)>` 拿 `Handle<Mesh>` → 按 id 查 `Assets<Mesh>` 提顶点/索引 → 上传 GPU 常驻池。**组件字段里的 Handle 是 ECS 世界指向资产世界的单向桥**。

## 概念盘点

World 字段五分组（被动仓库、三条读写路）/ Entity（index+generation u64 纯身份、NonMaxU32 哨兵、generation 防悬空）/ 万物统一身份（Resource/Observer 都是隐形实体）/ Resource（类型即键、唯一性=API 选型、insert 覆盖、三条多份出路）/ Assets\<A\>（Resource、双存储、Asset 开放 trait）/ Handle::Strong（Arc 保活）/ Handle::Uuid（跨运行稳定不保活）/ AssetId::Index vs Uuid / AssetServer 异步加载 / tick 变更检测（可变访问打 tick、帧末清窗口）/ Commands 入口（同步点详见参数篇）/ 数据放哪三层判定 / 同构表与单向桥（实体→资产）。
