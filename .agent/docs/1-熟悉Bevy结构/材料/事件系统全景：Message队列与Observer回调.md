# 事件系统全景：Message 队列与 Observer 回调

> 2026-09-18。基于本仓库 Bevy 0.19.1 源码（`crates/bevy_ecs/src/message/`、`crates/bevy_ecs/src/event/`、`crates/bevy_ecs/src/observer/`），并与 ProjectStorm（Unity DOTS）的 ECB 实体事件系统做对照。对应待深入清单的「Messages 双缓冲」和「Observer 与观察者传播」两项。

## 0. 一句话总览

Bevy 0.19 有**两套完全独立**的"事件"机制，解决两个不同的问题：

| | Messages（消息队列） | Event + Observer（触发回调） |
|---|---|---|
| 心智模型 | **发布-订阅广播**：写入缓冲区，消费者轮询 | **同步回调**：trigger 调用点立即执行观察者 |
| 生效时机 | 延迟：写后按调度顺序（或下一帧）被读 | 立即：`World::trigger` 调用栈内同步跑完 |
| 存储 | `Messages<M>` 资源，双缓冲 `Vec` | 观察者注册表（HashMap 查表），事件本身只是栈上值 |
| 消费次数 | 每 reader 恰好一次（游标去重） | 每个匹配的 observer 各跑一次 |
| 典型用途 | 帧级游戏事件流：伤害、拾取、动画事件 | 生命周期钩子、UI 点击冒泡、实体定向逻辑 |

> 历史注记：0.16 之前的 `Events<T>`/`EventReader`/`EventWriter` 就是今天的 `Messages<T>`/`MessageReader`/`MessageWriter`。改名是因为旧名把两套机制混在一个 `Event` 词上；0.19 起缓冲队列归 `Message`，同步触发归 `Event`。网上旧教程说 "Bevy 事件" 时多半指的是前者。

---

## 1. Messages：双缓冲 + 游标

### 1.1 数据结构

`Messages<M>` 是一个普通 Resource（`messages.rs:114`），核心是三个字段：

```rust
pub struct Messages<M: Message> {
    messages_a: MessageSequence<M>,   // 旧的（上一帧遗留）
    messages_b: MessageSequence<M>,   // 新的（本帧写入）
    message_count: usize,             // 全局单调递增计数
}

pub(crate) struct MessageSequence<M: Message> {
    messages: Vec<MessageInstance<M>>,    // 事件实体不存在，就是一个 Vec
    start_message_count: usize,           // 本缓冲第一条消息的全局编号
}
```

- 写入（`write`，`messages.rs:157`）：包成 `MessageInstance { message_id, message }` push 进 `messages_b`，`message_count += 1`。`MessageId` 携带全局编号和 `#[track_caller]` 调用点，可用于事后按 id 查消息（`get_message`）。
- **双缓冲不是 copy，是 swap**（`update`，`messages.rs:225`）：

```rust
pub fn update(&mut self) {
    core::mem::swap(&mut self.messages_a, &mut self.messages_b);
    self.messages_b.clear();
    self.messages_b.start_message_count = self.message_count;
}
```

每帧把"本帧缓冲"变成"上一帧缓冲"，清掉更老的那个。所以消息最多存活**两次 update**：生产帧 + 下一帧，帧末没被读就被静默丢弃。

### 1.2 读：游标 per-reader

`MessageCursor<M>` 只有一个字段 `last_message_count: usize`（`message_cursor.rs:39`）——"我读到了全局第几条"。读的时候从游标位置跨两个缓冲区顺序迭代，读完把游标推到 `message_count`。

关键推论：

- **广播语义**：每个 reader 有自己的游标，A 读过不影响 B；一个消息可被任意多个系统各消费一次。
- **恰好一次**：同一条消息对同一个 reader 只会出现一次，读第二次就是空。
- **两帧窗口**：连续两帧不读，游标落后于 `oldest_message_count`，消息就漏了（`missed_messages` 可以量化漏了多少）。
- `MessageReader<M>` 系统参数 = `Res<Messages<M>>` + `Local<MessageCursor<M>>`（游标状态存在系统内，不是 World 里）；`MessageMutator<M>` 则拿 `ResMut`，用于读写同类型消息（读写同用 Writer+Reader 会因资源冲突被调度器拒绝）。
- 并发规则：多个 `MessageReader<M>` 可并行；`MessageWriter<M>` 内部是 `ResMut<Messages<M>>`，同类型写者/写读之间互斥，由调度器自动串行化。`par_read` 可把未读消息并行迭代。

### 1.3 驱动：谁在调 update

`add_message::<M>()` 注册时把 `Messages<M>` 插为资源，并登记进 `MessageRegistry`；`message_update_system`（`update.rs:30`）每帧遍历 registry 对每种消息调 `update()`。它在 `MessageUpdateSystems` 系统集中，默认每帧跑；也支持信号模式（`signal_message_update_system`），切到 FixedUpdate 节奏。`add_message` 本身是个**同步点**（注册时插入资源和系统）。

### 1.4 用法示例（对齐 `examples/ecs/message.rs`）

```rust
#[derive(Message, Debug)]
struct DealDamage { pub amount: i32 }

// 写
fn deal_damage_over_time(mut writer: MessageWriter<DealDamage>, /* ... */) {
    writer.write(DealDamage { amount: 10 });
}

// 读（每帧调用，游标自动推进）
fn on_damage(mut reader: MessageReader<DealDamage>, query: Query<&Health>) {
    for dmg in reader.read() { /* ... */ }
}
```

顺序要点：写系统和读系统之间若没有显式 ordering，读系统可能在本帧也可能在下一帧读到——不会丢（双缓冲兜底），但会有最多一帧的不确定性。要确定性就 `.before()`/`.after()` 排序。

---

## 2. Event + Observer：触发即回调

### 2.1 基本闭环

```rust
#[derive(Event)]
struct Speak { message: String }

world.add_observer(|speak: On<Speak>| { println!("{}", speak.message); });
world.trigger(Speak { message: "Hello!".into() });   // 立刻打印，不等下一帧
```

三个角色：

- **`Event` trait**（`event/mod.rs:79`）：`Send + Sync + Sized + 'static`，带一个关联类型 `type Trigger<'a>: Trigger<Self>`——决定"谁被运行、拿到什么数据、什么顺序"。派生宏默认 `GlobalTrigger`。
- **`Observer`**：本身是一个**实体**，挂着 `Observer` 组件（`distributed_storage.rs:207`），组件里存一个类型擦除的 system（第一个参数必须是 `On<E>`）和一个 `last_trigger_id`。`world.add_observer(...)` 就是 spawn 这样一个实体。
- **`On<'w, 't, E, B>`**（`observer/system_param.rs:38`）：包装 `&mut E`——**观察者可以原地改事件内容**，后续观察者和 trigger 返回后都看得到修改。

### 2.2 触发路径（同步递归）

`World::trigger` → `trigger_ref_with_caller`（`observer/mod.rs:26` impl 处）：

1. `register_event_key::<E>()`：为每个事件类型注册一个内部组件 `EventWrapperComponent<E>`，拿它的 `ComponentId` 当 **`EventKey`**——事件类型的唯一 id 就是复用组件 id 系统，动态事件（无 Rust 类型）也能用 `EventKey::new(component_id)` 参与。
2. 查 `World.observers` 注册表拿 `CachedObservers`（`centralized_storage.rs:119`），三级结构：

```rust
pub struct CachedObservers {
    global_observers: ObserverMap,                              // 不看目标
    component_observers: HashMap<ComponentId, ...>,             // 看某组件类型
    entity_observers: EntityHashMap<ObserverMap>,               // 看某实体
}

pub struct Observers {   // 全局唯一
    add: CachedObservers, insert: ..., discard: ..., remove: ..., despawn: ..., // 高频生命周期事件免查表
    cache: HashMap<EventKey, CachedObservers>,
}
```

3. 按事件的 `Trigger` 策略遍历命中的 observer，逐个调用 `ObserverRunner`——一个 `unsafe fn(DeferredWorld, observer: Entity, &TriggerContext, event: PtrMut, trigger: PtrMut)`（`observer/runner.rs:22`）。runner 里取出 Observer 组件中的 system，downcast 回具体类型后同步执行。

**没有任何队列**：`trigger` 返回时所有 observer 都已跑完。事件数据就是 `trigger` 传入的栈上值，通过 `PtrMut` 传给每个观察者。

### 2.3 防重入与细节

- **`last_trigger_id` 守卫**（`observer/runner.rs:53`）：world 维护全局 `trigger_id`，每次触发自增；observer 若发现 `state.last_trigger_id == last_trigger` 就直接 return——同一次触发内不会因环状触发把自己再跑一遍。但"observer A 里 trigger 新事件 B"是合法的同步递归（深度需自己控制，例子里地雷连锁爆炸就是这么写的）。
- **run conditions**：observer 可以像系统一样挂条件（`examples/ecs/observers.rs:20`）。
- **改世界要走命令**：observer 拿到的是 `DeferredWorld`，写操作要 `commands` 排队到下一个同步点；读（Query、Res）是即时的。独占系统不能当 observer。
- **错误处理**：observer system 返回 `Result` 时走 fallback 或自定义 error handler。

### 2.4 EntityEvent：实体定向 + 层级冒泡

`#[derive(EntityEvent)]` 的事件带目标实体（自动识别 `entity` 字段或 `#[event_target]`），`EntityTrigger` 在跑完全局 observer 后，用目标实体**直接查 `entity_observers` 哈希表**，只跑挂在那个实体上的 observer——即 `world.entity_mut(e).observe(...)` 注册的实体级回调。这是 Bevy 的"定向事件 + per-entity 回调"。

`#[entity_event(propagate)]` 换成 `PropagateEntityTrigger`（`event/trigger.rs:296`）：沿 `Traversal`（默认 `ChildOf`，即向父级冒泡；可指定任意 relationship 组件）逐层重设 target 并触发，observer 可调 `click.propagate(false)` 中途截停——就是 UI 事件冒泡 + `stopPropagation`，auto_propagate 可默认开启。

### 2.5 内置生命周期事件

组件的 Add/Insert/Remove/Discard/Despawn 是 `EntityComponentsTrigger` 驱动的 EntityEvent：改动发生时在**改动的调用点**同步触发（不是帧末！），所以 `world.add_observer(|_: On<Add, Mine>| ...)` 能在 spawn 的同一行代码里生效。`Observers` 结构里这五个事件有专属的 `CachedObservers` 字段，就是为了免 HashMap 查表。

---

## 3. 一帧时序对比

```
Messages:
  [帧N Schedule运行]  写系统 --write--> messages_b
                      读系统(按调度顺序) --read--> 游标推进
  [帧末] message_update_system: swap 缓冲
  [帧N+1] 读系统仍能读到帧N的消息（另一缓冲未清）
  [帧N+1末] update 后帧N消息彻底消失

Event + Observer:
  trigger() 调用点 ──同步──> observer1 → observer2 → ... → 返回
  （生命周期事件则在组件增删的调用点同步触发）

ProjectStorm ECB 事件:
  [帧N 系统运行中]  SystemAPI.GetSingleton<EventSpawnSystem.Singleton>()
                    .CreateCommandBuffer(...).CreateEvent(DamageEvent{...})
                    ——只是记录命令，事件实体还没诞生
  [帧N LateSimulation末尾] EventSpawnSystem 回放：生成带 EntityEvent 标记的实体
  [帧N+1 各系统组] EventHandleSystemBase<T> 查询到实体，逐个 OnHandleEvent
                   处理中再发的 CreateEvent 又记入 ECB
  [帧N+1 LateSimulation末尾] EventDestroySystem 记销毁命令（下一帧回放时实体消失）
```

---

## 4. 对照 ProjectStorm：ECB 单帧实体事件

ProjectStorm 的事件系统（`Assets/Modules/Common/EntityEventSystem/`）是 Unity DOTS 的经典事件实体模式，四个角色：

| 文件 | 角色 |
|---|---|
| `EntityEventSystem.cs` | `EntityEvent` 空标记组件 + `EventSpawnSystem`（ECB 回放点，位于 `EntityEventSystemGroup` = `LateSimulationSystemGroup` 末尾）+ `EventDestroySystem`（帧末销毁全部 `EntityEvent` 实体） |
| `EventUtil.cs` / `EventEmitter.cs` | 发布封装：`GameUtil.E.CreateEvent<T>()`（EntityManager 版，不走 Burst）与 `EventSpawnSystem.Singleton.CreateEvent<T>()`（Burst 友好高频路径） |
| 事件本体 | `unmanaged IComponentData` struct，如 `DamageEvent { Value, Target, AttackType, IsCritical }` |
| `EventHandleSystemBase<T>` | 消费基类：每帧 `EntityQuery` 全部 `T` 事件实体，`ToComponentDataArray(TempJob)` 拷出后逐个虚调用 `OnHandleEvent` |

### 4.1 概念映射

| ProjectStorm | Bevy | 差异 |
|---|---|---|
| `struct DamageEvent : IComponentData` | `#[derive(Message)] struct DealDamage` | 都是值语义数据；DOTS 侧必须 `unmanaged` |
| `EventSpawnSystem.Singleton` + ECB 记录命令 | `MessageWriter::write`（push 进 Vec） | 你的是"先记账、回放点统一落盘"；Bevy 直接落盘 |
| `EventHandleSystemBase<T>`（一个事件类型一个消费者系统） | `MessageReader<T>`（一个系统可读多种，一个消息可被多个 reader 读） | 消费端形态不同，广播语义相同 |
| `EventDestroySystem` 销毁实体 = 消息出窗 | 双缓冲 swap，滑出两帧窗口 | 你靠"实体不存在了"，Bevy 靠计数游标 |
| 事件实体（archetype 分配/销毁） | Vec 里的 `MessageInstance` | Bevy 无实体开销 |
| `DamageEvent.Target` 字段 + handler 里手工 `Exists/HasComponent` 检查 | `EntityEvent` + `entity_observers` 哈希表直查 | 你是"带地址的数据"，Bevy 有真正的定向分发 |
| 无 | `On<E>` 可变事件、observer 条件、错误处理 | Bevy 回调链更可控 |
| 无游标，"实体存在即待处理" | `MessageCursor.last_message_count` | 见 4.2 |

### 4.2 三个本质差异

**① 生效时机：你的事件链每跳延迟一帧，Bevy 有"零延迟"选项。**
ProjectStorm 里 `HealthApplyDamageSystem` 处理 `DamageEvent` 时调用 `E.CreateEvent(new CharacterPropertyChangeEvent{...})`，这个新事件要到**下一帧**的 `EventSpawnSystem` 回放后才诞生，再下一帧才被消费——因果链上每多一跳就多一帧。Bevy Messages 同样是"写后需调度顺序保证"（不排序则最多延迟一帧），但 Observer 是 trigger 调用点同步递归，帧内闭环（如"扣血→判死→立即触发 DieEvent→死亡处理立即跑"一条栈内完成）。DOTS 里要做同步闭环，通常得手动立即回放 ECB 或改用 singleton 轮询，没有现成的"同步触发"原语。

**② 消费判定：存在性查询 vs 游标计数。**
`EventHandleSystemBase` 的隐含协议是"事件实体活着 = 没处理过"，所以必须保证事件只活一帧，且**所有**消费者系统都必须在这一帧窗口内查到它——漏了不是"下帧再读"而是永久丢失（下帧实体已销毁）。Bevy 的游标把"读没读"从世界状态（实体存在性）挪到了读方私有状态，消息有两帧宽的容错窗口，读者系统迟一帧也不丢；代价是每个读者都要每帧跑一次才算"尽责"。另外你的查询"不区分新旧帧"（CodeDoc 里自己注明了）：若事件实体生命周期被调长，同一事件会被重复回调——游标模型天然免疫这个问题。

**③ 消费端成本：托管拷贝回调 vs 零分配迭代。**
`EventHandleSystemBase` 是 `SystemBase`（托管），每帧 `CalculateEntityCount` + `ToComponentDataArray(Allocator.TempJob)` 堆分配 + `foreach` 虚调用，写侧 Burst 了读侧没 Burst。Bevy 的 `reader.read()` 是裸 slice 上的迭代器，零分配零虚调用；调度器还按 `Res/ResMut` 冲突自动并行化多个只读消费者。不过要公平地说：DOTS 事件实体模式的优势是**事件本身进了 ECS 数据模型**——可以被 Query 统计、过滤、Burst 系统批量扫（你的事件实体是普通实体，任何系统都能 join 它）；Bevy Messages 在 Burst/Job 世界里没有对应物，`Messages` 就是主线程资源。这是两套生态约束下的合理选择，不是谁更差。

### 4.3 相同的设计判断

两边在几处做出了同样的取舍，值得记下来：

- **广播而非路由**：一个事件多个消费者，各拿一份，互不干扰（你：多个 `EventHandleSystemBase<DamageEvent>`；Bevy：多个 reader）。
- **发布者不关心消费者存在**：都是纯匿名广播，无 request-response。
- **定向靠数据字段**：`Target: Entity` 是消费者自己 interpretation 的（你手工查 Target 组件）；Bevy 虽然有 `EntityEvent` 分发，但全局 observer 照样能收到所有实体的事件，两级共存。
- **消费者内再发事件是合法且常见的**（你的 `HealthApplyDamageSystem` → `CharacterPropertyChangeEvent`；Bevy observer → `commands.trigger`）。

---

## 5. 选型口诀

- **帧级"发生了什么"流水**（伤害、拾取、UI 刷新信号）→ `Message`：可缓冲、可多读、可延迟、可并行。
- **"此刻必须发生"的反应**（组件生命周期、点击命中某实体、层级冒泡）→ `Event + Observer`：同步、定向、可改数据、可截断。
- 拿不准时的 Bevy 官方倾向：轮询流水用 Message，世界结构变化和实体定向逻辑用 Observer；UI（bevy_ui）几乎全走 EntityEvent 冒泡。

## 6. 给 ProjectStorm 的三条可借鉴点

1. **读者侧去重窗口**：`EventHandleSystemBase` 若改为记录"已处理的 entity 索引/Generation"（或事件实体加 `Frame` 组件标注出生帧），事件生命周期就可以放宽到 N 帧，消费者系统的执行顺序就不必都挤在事件存活帧内。
2. **消除事件链的逐跳延迟**：对"同一系统组内的因果闭环"（伤害→死亡→掉落），可以在消费回调里对紧急下游事件做 immediate playback（单独的同步 ECB），或像 Bevy Observer 一样把"必须帧内闭环"的逻辑改成直接函数调用而非事件。
3. **消费端去分配化**：`ToComponentDataArray(TempJob)` 每帧分配可换成 `foreach (var (evt, e) in SystemAPI.Query<RefRO<T>>().WithEntityAccess())` 直接迭代 chunk，配合把 `OnHandleEvent` 改成 static + Burst 可调用（`[BurstCompile]` 静态方法处理数据部分），消掉托管虚调用与每帧 GC 分配。

## 附：源码索引

| 内容 | 位置 |
|---|---|
| `Messages` 双缓冲、write/update | `crates/bevy_ecs/src/message/messages.rs` |
| 游标 | `crates/bevy_ecs/src/message/message_cursor.rs` |
| Writer/Mutator 系统参数 | `crates/bevy_ecs/src/message/message_writer.rs`、`message_mutator.rs` |
| 每帧 update 驱动 | `crates/bevy_ecs/src/message/update.rs` |
| `Event`/`EntityEvent` trait 与文档 | `crates/bevy_ecs/src/event/mod.rs` |
| Trigger 策略（全局/实体/冒泡/组件生命周期） | `crates/bevy_ecs/src/event/trigger.rs` |
| Observer 组件与 runner（防重入） | `crates/bevy_ecs/src/observer/distributed_storage.rs`、`observer/runner.rs` |
| 观察者注册表 | `crates/bevy_ecs/src/observer/centralized_storage.rs` |
| `On` 包装 | `crates/bevy_ecs/src/observer/system_param.rs` |
| 官方示例 | `examples/ecs/message.rs`、`examples/ecs/observers.rs`、`examples/ecs/observer_propagation.rs` |
| ProjectStorm 事件系统 | `F:\okzkx\Unity-Project-Storm\Assets\Modules\Common\EntityEventSystem\` |
