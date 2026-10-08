# 实体写路径：Bundle、Commands与Query改值

> 2026-09-17 建。收编三轮问答（spawn 的 bundle 是什么 / Commands 用法与"不走 Commands"的路径 / Query 出的组件能不能改），收敛成一个主题：**改 World 只有两条路——结构路径（Commands：增删实体组件，延迟回放、走 hook）与值路径（Query `&mut`：原地写值，立即生效、自动标脏）**。bundle 是结构路径的"出生配置单"。同步点三时机与手动控制见《System参数与数据访问》§3（本文不重复），调度与并行规则见《System与调度DSL》，数据模型见《数据层全景》。

## 1. Bundle：spawn 的"出生组件清单"

一句话：**bundle = 实体出生时静态声明的组件集合**。Entity 只是 ID，bundle 才是它的血肉。

### 1.1 类型层面：一张编译期的组件清单

`Commands::spawn<T: Bundle>`（commands/mod.rs:398）收一个 `Bundle`，trait 本体只有一个核心方法（bundle/mod.rs:207）：

```rust
pub unsafe trait Bundle: DynamicBundle + Send + Sync + 'static {
    fn component_ids(components: &mut ComponentsRegistrator)
        -> impl Iterator<Item = ComponentId> + use<Self>;
}
```

bundle 的**类型本身**携带"我包含哪些组件"的完整清单，按固定顺序返回所有 `ComponentId`。实现规则：

- **不能手写 impl**（unsafe trait，无安全实现途径），只能 `#[derive(Bundle)]`（字段须为组件或另一个 bundle）或用 tuple；
- 任意 `Component` 单独就是一个 bundle（所以 `spawn(ComponentA(1))` 直接传单组件）；tuple 最多 15 个，可嵌套；`()` 是空 bundle（配 `spawn_batch` 造空实体）。

### 1.2 为什么存在：一步落位 archetype

组件组合编译期已知 → spawn 时直接找到（或创建）**恰好拥有这些列的 archetype 表**，实体一次性写入——而不是先建空实体再逐个 insert、每插一个搬一次表。bundle 概念的根本理由：**把 N 次插入折叠成 1 次按表落位**。

**Unity/DOTS 映射**：≈ `EntityManager.CreateEntity(entityArchetype)`，但 archetype 不用运行时手工建——从 tuple/struct 类型自动推导。`spawn((Transform, Mesh3d, Handle<Material>))` ≈ Instantiate 同时挂好全部组件、直接归入正确的原型桶。

### 1.3 两条认知红线

1. **Bundle 不是行为单元**。bundle/mod.rs:105 原话 "bundles are only their constituent set of components. You should not use bundles as a unit of behavior"——Bevy 故意不提供"实体是否拥有某 bundle"的 Query，系统匹配的永远是组件组合。两个 bundle 都含 `Health`（各自算逻辑不同）同时挂同一实体 = 后挂静默覆盖。`#[derive(Bundle)]` 的 `PlayerBundle` 是"组件预设"，不是 prefab 那种逻辑边界。
2. **Bundle 同时是移除单位**。`remove::<PlayerBundle>()` 把实体上该 bundle 包含的组件逐个清掉（不要求凑齐全部）。

### 1.4 Commands::spawn 的额外语义

spawn 当场从 `EntityAllocator` 分配 Entity ID（`EntityCommands`/`.id()` 立即可用），bundle **值被 move 进命令队列**（commands/mod.rs:401-406），下个同步点才落表。同帧稍早的系统此刻查不到组件；要立即落表用 `world.spawn`（同一 bundle 语义，只差插入时机）。

## 2. Commands 用法全貌

定位（commands/mod.rs:42）："A Command queue to perform structural changes to the World"——结构修改需要 `&mut World` 独占访问，普通系统只能拿互不重叠的借用才能并行，所以系统运行期只往队列塞闭包，调度器在同步点独占 World 时串行回放（回放时机见《System参数与数据访问》§3 三时机，默认自动插 `ApplyDeferred`）。

### 2.1 API 按组记（0.19.1 实测 commands/mod.rs）

**队列级（Commands 本体）**：

| 组 | 方法 |
|---|---|
| 实体 | `spawn(bundle)` / `spawn_empty()` / `spawn_batch(iter)` / `entity(e)` / `get_entity(e)`（Result 版） |
| 批量插入 | `insert_batch` / `insert_batch_if_new` / `try_insert_batch`（给一堆既有实体） |
| 资源 | `init_resource` / `insert_resource` / `insert_resource_if_neq` / `remove_resource` |
| 事件观察者 | `trigger(event)` / `add_observer(obs)` / `write_message(msg)`（0.19 消息/事件分野见《System参数与数据访问》§2.4） |
| 子系统驱动 | `run_system(id)` / `run_system_cached(sys)` / `register_system` / `run_schedule(label)` |
| 自定义 | `queue(cmd)` / `queue_handled` / `queue_silenced` / `append` |

**实体级（EntityCommands，链式）**：`insert`（含 `_if`/`_if_new`/`_if_neq` 变体）、`try_insert`、`remove`/`try_remove`/`remove_by_id`/`remove_with_requires`/`retain`、`clear`、`despawn`/`try_despawn`、`entry<T>()`（or_insert/or_default/and_modify 的 upsert 语法）、`observe`、`clone_and_spawn`（含 opt_in/opt_out 变体）、`clone_components`/`move_components`（实体间搬组件）、`add_related::<R>`/`insert_related`（关系批量挂子）。

### 2.2 错误处理与 try_ 家族

命令可返回 `Result`：错误交给 error handler（默认 panic，可经 `FallbackErrorHandler` 资源全局换）；`queue_handled` 单命令定制 handler，`queue_silenced` 吞掉。`try_insert`/`try_despawn` 系列 = "实体已死不炸、返回 Err"的温和版，适合可能被别处 despawn 的实体。

**Unity 类比**：Commands ≈ DOTS 的 EntityCommandBuffer（先录制后回放），`ApplyDeferred` ≈ 播放点，schedule 末尾 ≈ 帧边界强制提交；`queue(closure)` ≈ 往 ECB 塞自定义 EntityCommand。

## 3. 不走 Commands：独占系统立即写

普通系统里没有第二条路做结构修改——`&mut World` 与其它参数的借用天然冲突，这正是 Commands 存在的原因。替代方案是把系统声明成**独占系统**：普通函数只要带 `&mut World` 参数，**自动**成为 exclusive system（system/mod.rs 有 `is_exclusive()` 测试佐证）：

```rust
fn spawn_enemies_immediately(world: &mut World) {
    let e = world.spawn((Enemy, Health(50))).id();  // 立即落 archetype，当场生效
    let hp = world.get::<Health>(e).unwrap();        // 同函数内马上读回，没有同步点
}
```

- **收益**：修改立即生效可同步读写回；可批量程序化构建（初始化/测试/生成关卡）；可 `world.run_schedule` 驱动别的调度。可搭配 `In<I>`、`Local<T>`、`&mut SystemState<P>`。
- **代价**：独占系统与**一切其他系统互斥**，调度器在它前后断开并行流水——它是串行段。≈ Unity 里回主线程用 EntityManager 直接改结构。

**中间形态**：`ParallelCommands`（`Query::par_iter` 等并行上下文里每线程一个局部队列，最后合并，不必退化成独占）；`commands.queue(|world: &mut World| {...})`（拿到 `&mut World` 但仍在延迟体系内）；`Deferred<T>`（自定义 SystemParam 的延迟缓冲原语，Commands 内部即 `Deferred<CommandQueue>`，Gizmos 同构案例见《System参数与数据访问》§2.7）。

**选型口诀**：默认 `mut commands: Commands` 保住并行；"必须立即读回"或"批量程序化构建"才付独占的串行代价。

## 4. Query 改值：另一条写路径

### 4.1 能不能改由查询签名决定

```rust
fn read_only(q: Query<&Health>)          { /* 拿到 Ref<Health>，改不了 */ }
fn writable(mut q: Query<&mut Health>) {
    for mut hp in &mut q { hp.0 -= 10; } // 原地改，立即生效
}
```

- `&T` → `Ref<T>`（params.rs:665）：内部只包 `&'w T`，**没有 `DerefMut`**——类型系统直接禁止只读查询里偷改；
- `&mut T` → `Mut<T>`（params.rs:908）：实现 `DerefMut`，解引用即写；
- **Immutable 组件例外**：组件可声明 `Mutability = Immutable`（`ComponentMutability` trait，component/mod.rs:688-697），此类组件 `&mut` 访问直接编译不过，只能走 insert/remove 路径"改"——通常是引擎用 hook 自维护的数据，防止绕过维护逻辑。

单实体定点写：`q.get_mut(entity)`、`q.single_mut()`、查询数据里混 `Option<&mut T>`。

### 4.2 自动变更检测

`&mut` 写入自动更新 changed tick，下游 `Query<&T, Changed<T>>` 直接可见——相当于自动标脏（DOTS 手动 dirty flag 的免费版）。细控：`set_changed()` 手动标记、`set_if_neq()` 值相等免标记、`bypass_change_detection()` 完全旁路（params.rs:1365/1395）。

## 5. 两条写路径对比（本篇收敛结论）

| | 结构路径（Commands / insert / remove / spawn / despawn） | 值路径（Query `&mut` 写值） |
|---|---|---|
| 生效时机 | 同步点回放（延迟） | 立即，同函数可读回 |
| archetype | 可能搬表（列集合变） | 原表原列 |
| hooks/observers | 触发 on_insert/on_remove 等 | 不触发 |
| 变更检测 | insert 更新 ticks | 自动更新，可 bypass |
| 并行代价 | 系统本体零 World 写访，全并行 | `&mut` 是写访问，限制并行 |
| 失败语义 | despawn 后 ID 失效，try_ 家族兜底 | 借用规则编译期保证 |
| Unity/DOTS | 结构性更改（ECB / EntityManager） | 直接写 component data（改 Component 字段） |

**同系统冲突**：两个查询重叠持有同组件写权限 → 初始化 panic（错误码 B0001，errors/B0001.md；system/mod.rs:855 有测试），用 `Without` 把实体集划成不相交即解。**跨系统冲突**：不报错，调度器把访问冲突的系统串行化——所以**写访问是并行性资源**，只读字段尽量 `&T`（组件维度判定细则见《System参数与数据访问》§2.8）。

**边界情况**：`insert` 到已拥有该组件的实体，列集合不变、不搬表，实质是"延迟的值替换"——但仍走 `on_replace` hook、走命令队列。判断标准不是"值变没变"，而是**走哪条机制**：凡 insert/remove/spawn/despawn 这套 API 都算结构路径，`&mut` 永远是值路径。

**决策口诀**：改数据 → `&mut` 查询；改"实体有什么数据" → Commands；要立即读回 → 独占系统。

## 相关阅读

- 《System参数与数据访问.md》——同步点三时机与手动控制、参数目录、多 Query 冲突判定、Gizmos 的 Deferred 同构
- 《System与调度DSL.md》——同步点为何能独占 World（调度/执行机制）
- 《数据层全景：World、Entity、Resource与Asset.md》——archetype 表结构与 Entity 位布局
- 示例：`examples/ecs/ecs_guide.rs`、`event.rs`、`one_off.rs`
