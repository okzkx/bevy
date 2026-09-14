# World 与 Resource：数据层全景

> 2026-09-14 建。承接《Bevy结构笔记》第 1 节（World 曾一句话带过），本篇讲透 World/Resource 及其牵出的两个机制（变更检测 tick、Commands 延迟）。文末附**基础概念盘点**——查遗漏用。

## 1. World：数据总仓库（字段级解剖）

`crates/bevy_ecs/src/world/mod.rs:98-115`，字段按职责分五组：

| 组 | 字段 | 职责 |
|---|---|---|
| 实体 | `entities: Entities`、`entity_allocator` | 实体 id 分配与回收（带世代防复用悬空） |
| 存储表 | `components`、`archetypes`、`storages`、`bundles` | 组件的表结构（原型 = 组件组合，表 = 列式存储）——Query 遍历的数据底座 |
| **资源** | `components` + **`resource_entities`** | 见第 2 节：资源内部就是"挂在隐形实体上的组件" |
| 观察者 | `observers`、`removed_components` | Observer 回调注册表、组件移除消息 |
| 变更检测 | `change_tick: AtomicU32`、`last_change_tick`、`last_check_tick` | 见第 3 节：全局 tick 就存在 World 里 |
| 命令队列 | `command_queue: RawCommandQueue` | 见第 4 节：Commands 的暂存地 |

**三个要点**：

1. **World 是被动的**：它不跑逻辑。"世界在动"= 挂在它上面的 Schedule 在跑。
2. **读写只有三条路**：① 系统参数（常态，调度器管借用安全）；② `World` API 直接操作（`world.spawn()` / `world.resource_mut::<T>()`——用于引导代码、测试、一次性脚本，`externally_driven_headless_renderer.rs` 里 `self.0.main.world_mut().spawn(...)` 就是范例）；③ 独占系统（`&mut World` 整借，牺牲并行换自由）。
3. **与 Unity 类比再校准**：World ≈ "所有已加载场景的对象仓库 + 全局单例"合并成的数据容器；Unity 的场景切换在 Bevy 里只是"对 World 批量 spawn/despawn"。

## 2. Resource：全局单例（且底层就是组件）

**0.19.1 的关键事实**（`crates/bevy_ecs/src/resource.rs:87`）：

```rust
pub trait Resource: Component {}
```

**Resource 是 Component 的子 trait**。每个资源内部对应一个**隐形实体**（`resource_entities` 缓存维护 ComponentId → Entity 的链接）。这不是冷知识——它统一解释了：

- 资源天然参与变更检测（`Changed<Res<T>>` 有效）；
- 资源天然能被 Observer 观察、能被反射系统统一处理；
- 一套存储机制服务两种数据，代码更少。

**用法速查**：

- 声明：`#[derive(Resource)]`；注入：`app.init_resource::<T>()`（Default）/ `insert_resource`（带值）；
- 系统参数：`Res<T>`（读）/ `ResMut<T>`（写，打 Changed）/ `Option<Res<T>>`（容忍缺失）；
- **选择标准一句话：全游戏只有一份的用 Resource，有 N 份的用 Component**。"每个敌人的血量"是组件，"游戏时间"是资源；
- 常见内置资源：`Time`、`Assets<T>`、`Schedules`（调度器本身！）、`MainScheduleOrder`、`AppTypeRegistry`、`ClearColor`；
- **`NonSend<T>`/`NonSendMut<T>`**：非线程安全资源，只能在主线程系统访问——`WinitWindows` 就是（第二步拿窗口句柄要用的 NonSend 资源）；
- **`Local<T>`**：系统级"私有资源"（按系统实例独立一份，跨帧保留）——hello_world 里的计数器就是。

## 3. 变更检测的落点：tick 就在 World 字段里

- `change_tick: AtomicU32` 是全 World 的逻辑时钟；每次对组件/资源的**可变访问**（`DerefMut`、`&mut`）都会把该数据的 tick 打成当前值；
- 系统自带 `last_run`/`this_run` tick 窗口：`Changed<T>` = 数据 tick 落在窗口内；
- 帧末 `main.world.clear_trackers()`（`SubApps::update` 末尾，见《SubApp机制与取舍》第 2 节）清窗口——extract 必须赶在它之前，这也是 extract 时序契约的底层原因。

## 4. Commands 与延迟生效（易错点）

- `Commands` 参数不立即改 World：操作进 `command_queue`，之后批量应用；
- **调度器默认自动插同步点**（`ScheduleBuildSettings::auto_insert_apply_deferred`，`schedule.rs:517-523`）：有延迟参数的系统之后自动插 `apply_deferred`；
- **对渲染采集的直接含义**：`Commands::spawn` 的实体当帧 Query 看不到，要等同步点之后（或下一帧）才可见——采集系统别假设"同系统里 spawn 就能查到"。

## 5. 基础概念盘点（遗漏检查，2026-09-14）

| 概念 | 状态 | 位置 |
|---|---|---|
| App / runner / 生命周期 | ✅ | Bevy结构笔记 §3、§5 |
| SubApp / Extract / 双 World | ✅ | SubApp机制与取舍 |
| Schedule / Main 顺序 / 变换传播 | ✅ | Bevy结构笔记 §2 |
| Plugin / PluginGroup / 宏展开 | ✅ | Plugin与PluginGroup机制 |
| World / Resource / tick / Commands | ✅ | **本篇** |
| System 与系统参数全家桶（Query 过滤器、ParamSet、Exclusive） | ⬜ 散落提及，未成篇 | — |
| Messages（双缓冲、Reader/Writer 游标） | ⬜ 简述过 | 待读 `examples/ecs/message.rs` |
| Observer 与观察者传播 | ⬜ 简述过 | 待读 `examples/ecs/observers.rs` |
| 关系与层级（ChildOf/Children/传播细节） | ⬜ 简述过 | 待读 `examples/ecs/hierarchy.rs` |
| 资产系统（AssetServer/Handle/异步/事件/热重载） | ⬜ 简述过 | = 第一步 Q4 |
| 状态机 State + 定点步 RunFixedMainLoop | ⬜ | 大世界阶段需要（按天分帧） |
| DefaultPlugins 三分类清单 | ⬜ = 第一步 Q2 | 待做 |

**优先级建议**：下一步优先补 **System 参数全家桶 + Commands**——第三步写渲染采集系统时天天要用；Messages/Observer 在抄 bevy_city 加载流时补；资产系统跟 Q4 一起补。
