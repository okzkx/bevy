# System 参数与数据访问

> 2026-09-16 建。收编 README 待深入①（System 参数全家桶）②（Commands 同步点）两轮讨论：0.19.1 参数全量目录 + 核心语义 + 高频问答。参数是系统的"接口声明"——**访问权限写在参数类型里**，调度器靠它排并行（构图/执行机制见《System与调度DSL》）。数据模型（World/Resource/Entity/Asset）见《数据层全景》。

## 1. 参数是什么：接口即访问声明

三条先记住的规则：

1. **参数元组本身也是 SystemParam**——宏生成 1~16 元 impl（function_system.rs:874），超 16 元拆系统或用 `ParamSet`；
2. **权限编码在类型里，不在 `mut` 里**：`Res` vs `ResMut` 是类型选择（调度器编译期读到读写集）；`mut x: T` 的 `mut` 只是绑定可变性，不是类型的一部分；
3. **`'w`/`'s` 双生命周期**：`'w` = 借 World 数据，`'s` = 系统私有状态（Query 缓存、Local、命令队列都住这）。

数据三来源总览：

| 来源 | 参数 | 特征 |
|---|---|---|
| World 借·单例 | `Res`/`ResMut` | 全局一份，类型即键 |
| World 借·复数 | `Query` | 按 (实体，组件) 迭代，带过滤器 |
| 系统私有 | `Local` | 每系统实例一份，按值拥有，调度免费 |
| 验证型便捷 | `Single`/`Populated` | 不满足条件→整个系统跳过 |

## 2. 全量参数目录（0.19.1 实测源码）

### 2.1 组件访问：Query 家族（最高频）

```rust
Query<D, F>   // D=取数模板（QueryData），F=过滤模板（QueryFilter）
```

**D 能放**：`&T`、`&mut T`、`Entity`、`Option<&T>`、`Option<&mut T>`、`()`（纯过滤不取数）、**元组（AND）**、`AnyOf<(A, B)>`（OR 取数）、`Has<T>`（bool）、关系组件直接取（`&ChildOf` 父 / `&Children` 子）。

**F 能放**：`With<T>`、`Without<T>`、`Added<T>`、`Changed<T>`、`Spawned<T>`、`Or<(…)>`、`Has<T>`、**元组（AND）**。

**单实体便捷参数**（验证机制，失败=系统跳过，不是 panic）：

- `Single<D, F>`——恰好一个匹配才给（Deref 直取字段）；0 个或 ≥2 个 → 系统跳过（query.rs:2810）。0 或 1 可容时用 `Option<Single<D, F>>`；
- `Populated<D, F>`——非空才跑，空集 → 跳过（query.rs:2892）。纯判空想省 O(n) 验证用 run_if 条件 `any_match_filter`。

### 2.2 资源访问

| 参数 | 行为 |
|---|---|
| `Res<T>` / `ResMut<T>` | 缺失 = 验证失败：默认 panic，0.19 起可配置 no-op/warn once（params.rs:459-461） |
| `Option<Res<T>>` / `Option<ResMut<T>>` | 容忍缺失 |
| `NonSend<T>` / `NonSendMut<T>` | 非 Send 资源，系统钉主线程（NON_SEND 旗标） |
| `Local<T: FromWorld>` | 系统私有自有状态，见 §4 |
| `FilteredResources` / `FilteredResourcesMut` | 受白名单限制的资源视图 |
| `&World` | 只读全视图（system_param.rs:785，唯一的引用型 World 参数） |

### 2.3 命令与延迟生效

- `Commands`——命令队列，同步点落账，见 §3；
- `ParallelCommands`——多线程分块命令（`.parallel_scope()`），不必退化成独占系统；
- `Deferred<T: SystemBuffer>`——自定义缓冲参数（进阶）。

### 2.4 消息与事件（0.19 命名分野）

**缓冲消息**（双缓冲+游标，跨帧读）：`MessageReader<M>` / `MessageWriter<M>`，类型 `#[derive(Message)]`——旧名 `EventReader/EventWriter` 已废弃改名。

**观察者事件**（触发即跑）：`On<E, B>` 参数（observer/system_param.rs:38）——形如 `fn on_add_mine(add: On<Add, Mine>, query: Query<&Mine>, ...)`（examples/ecs/observers.rs:142）。

**组件删除检测**：`RemovedComponents<T>`。

### 2.5 独占类（拿 `&mut World`）

`&mut World` 出现 → 系统带 EXCLUSIVE 旗标 = 调度屏障，与一切互斥。可搭配的独占参数家族：`Local`、`&mut SystemState<P>`、元组（exclusive_system_param.rs:40-120）。

### 2.6 工具与自定义

- `SystemChangeTick`——本系统上次运行 tick，手写跨参数变更检测；
- `ParamSet<P>`——**多个 Query 访问冲突时的互斥轮借**（`p0()`/`p1()` 依次借）；
- `QueryLens<Q, F>`——组合子：从已有 Query 变造（transmute）子查询；
- `PhantomData`——泛型占位，零访问；
- `#[derive(SystemParam)]`——把一组参数打成自定义结构体（每字段须是 SystemParam）。

### 2.7 插件型参数：Gizmos 即样板（含判别口诀，重点）

**判别口诀（重点）**——参数位置看到一个类型，先问一句：**它是 World 里的数据，还是实现了 `SystemParam` 的访问器？**

| 类别 | 是什么 | 参数位置 |
|---|---|---|
| **数据类型**（住 World 里） | `Time`、`Assets<T>`、`Transform`、各种组件 | ❌ 必须经访问器包（`Res<Time>`） |
| **参数类型**（实现 `SystemParam`） | `Res`/`ResMut`/`Query`/`Commands`/`Local`/`Single`/`ParamSet`/`&World`…——**bevy_ecs 预置** | ✅ 裸放（它们本身就是参数本体） |
| 同上，**插件自造** | **`Gizmos`**（bevy_gizmos） | ✅ 裸放 |

所以"`mut gizmos: Gizmos` 能裸写"与"`Assets<T>` 不能裸写"是同一条规则的两个方向：**数据类型不能裸传，参数类型只能裸传**。

**Gizmos 内部构造实拍**（gizmos.rs:143/178/195，0.19.1 已核实）——插件用两个原语参数手工组装：

```rust
pub struct Gizmos<'w, 's, Config, Clear> {
    buffer: Deferred<'s, GizmoBuffer<Config, Clear>>,  // 缓冲：同 Commands 的队列模式
    pub config: &'w GizmoConfig,
    pub config_ext: &'w Config,
}
// SystemParam impl 直接委托给参数元组：
type GizmosState<Config, Clear> = (
    Deferred<'static, GizmoBuffer<Config, Clear>>,   // §2.3 那个"进阶" Deferred 的真实用武之地
    Res<'static, GizmoConfigStore>,                  // 只读配置（get_param 里查 enabled 做廉价早退）
);
```

三个要点：① SystemParam 是开放协议，任何 crate 都能发明新参数形态（手写 `unsafe impl` 或 `#[derive(SystemParam)]`）；② 它声明为只读（安全注释："Each field is ReadOnlySystemParam…does not mutate world"）——从调度器视角不与任何东西冲突，系统全并行，真正的"写"在缓冲内部、同步点统一落账；③ `Option<Gizmos>` 也是合法参数——配置组没初始化时整个系统跳过（与 `Option<Res>` 同款语义）。

**模式总结**：`Gizmos` 之于 gizmo 缓冲 = `Commands` 之于命令队列——"Deferred 缓冲 + 只读声明 + 同步点落账"三件套同构。

**怎么快速确认一个新类型行不行**：查 rustdoc "Trait Implementations" 有无 `SystemParam`；或直接写，不实现会被 `on_unimplemented` 的友好错误拦住（"`X` does not describe a valid system configuration"）。

**项目落点**：M2 起给渲染器定义 `RenderContext<'w>` 参数（内部借走 mesh/texture 池），采集系统写 `fn upload(ctx: RenderContext, meshes: Res<Assets<Mesh>>)`。

### 2.8 多 Query 共存规则（atmosphere_controls 案例）

同一系统多个 `Query` **必须组件级互不冲突**，否则 initialize 时 panic（报错让用 ParamSet）。`Query<(&mut Atmosphere, &mut GlobalTransform)>` + `Query<&mut AtmosphereSettings, With<Camera3d>>` + `Query<&mut Exposure, With<Camera3d>>` 能共存：写的组件两两不同——**即使命中实体集重叠**（都可能是同一台相机）也无冲突。冲突判定是**组件维度不是实体维度**。

**Query vs Single 选型**：单例强保证用 `Single`（数不对系统跳过）；逻辑必须每帧都跑（键盘处理等）用 `Query + With`，零台就空转。

## 3. Commands 什么时候执行（同步点全解）

**结构**（commands/mod.rs:105）：`Commands<'w, 's>` 的队列存系统私有状态（'s），借 World 的 `Entities`/`EntityAllocator`。

**系统运行期间对 World 零写入**：`commands.spawn(...)` 只做两件事——① 向 `Entities` **预定** Entity ID（立即有效，可马上建父子关系）；② 把挂组件的操作压进本系统队列。组件此时刻不存在。

**落账三时机**（按先后）：

1. **依赖边上的自动 `ApplyDeferred`**：`auto_insert_apply_deferred` build pass 跑"同步距离"算法（auto_insert_apply_deferred.rs:120-215）——给每个系统记 distance（前面隔了几个同步点），DEFERRED 系统的出边插 `ApplyDeferred`，且**多个 deferred 系统合并到同一同步点**；`*_ignore_deferred` 边免插；
2. **显式 `ApplyDeferred`**：它就是个普通屏障系统——`.add_systems(Update, (a, ApplyDeferred, b).chain())`，b 必然看得见 a 的命令；
3. **schedule 末尾 final apply**：两 executor 的 `run()` 结尾 `apply_final_deferred`（默认 true）清掉全部未落账队列——**跨 schedule 必然可见**（Update spawn 的实体 PostUpdate 一定看得见；SpawnScene 卡 Update/PostUpdate 之间的前提）。

**两 executor 的差别只在同步点怎么跑**：

- 多线程（multi_threaded.rs:700-708）：`ApplyDeferred` 当独占任务跑，收集 `unapplied_systems` 逐个 flush，全池屏障；
- 单线程（single_threaded.rs:139-165）：同样不逐系统立即落账——普通系统走 `run_without_applying_deferred` 进 `unapplied_systems`，到同步点或末尾统一 flush。

**依赖规则**：同 schedule 内无顺序关系的系统命令**不可依赖**；有 before/after 边的可依赖（同步点已自动插）；跨 schedule 必可依赖。

**手动控制全家**：`commands.queue(|world: &mut World| { ... })` 闭包命令（可返回 `Result` 走错误处理，`queue_handled` 定制 handler）；`before/after/chain_ignore_deferred` 让命令延到 schedule 末尾；独占系统里 `world.flush()` 立即落账；关 `ScheduleBuildSettings::auto_insert_apply_deferred` 全局免插（坑：chain 也不插了，命令全部延到末尾）。

**Unity 类比**：Commands ≈ 录制的操作列表，事务提交点才回放；`ApplyDeferred` ≈ 提交点；schedule 末尾 ≈ 帧边界强制提交。

**两条写路径的分工（setup 四参数例，examples/3d/3d_shapes.rs:56）**：

```rust
mut commands: Commands,                        // ① 延迟写实体
mut meshes: ResMut<Assets<Mesh>>,              // ② 立即写网格资产
mut images: ResMut<Assets<Image>>,             // ③ 立即写贴图资产
mut materials: ResMut<Assets<StandardMaterial>>, // ④ 立即写材质资产
```

分工由"**要不要返回值**"决定：资产走 `ResMut<Assets<X>>` **直改**——`Assets::add` 当场返回 `Handle` 值马上要拼组件，命令拿不到返回值所以必须同步；实体走 `Commands` **延迟**——不急着读回，成批提交更便宜。访问集账单：写三个 Assets 资源（互写者串行）+ DEFERRED 旗标。注意资产入库只是 CPU 仓库，**GPU 上传不在这条链路里**——那是 M2 渲染器 extract 阶段的事。`Res<AssetServer>`（只读入口，`load` 返回 Handle 立即有效、数据异步到货）与 `ResMut<Assets<X>>`（仓库本体，add 当场入库）是"一入口一仓库、Handle 衔接"。

## 4. Local 详解

定义（system_param.rs:973）：

```rust
pub struct Local<'s, T: FromWorld + Send + 'static>(pub(crate) &'s mut T);
```

**系统私有的、跨帧存活的成员变量**——Bevy 系统是普通函数，没有闭包捕获、没有 this，`Local` 就是"给这个系统一块私有小本本"的声明式替代。

- **生命周期挂 `'s`**：值存 `FunctionSystemState`，随系统生死——跨帧存活不需要 World；
- **初值来自 `FromWorld`**：world/mod.rs:4000 有 blanket impl `impl<T: Default> FromWorld for T`，所以 `Local<Stopwatch>` 首帧自动 `default()`；需要 World 上下文的初值自己 `impl FromWorld`；
- **每系统实例一份**：同函数 add 两次 = 两份独立 `Local`；共享必须 `Res`；
- **调度完全免费**：不登记 World 访问，不参与冲突判定——与 `Res<T>` 的互斥语义对照鲜明；
- **例证**：`Main::run_main(world, run_at_least_once: Local<bool>)`（main_schedule.rs:290）——"Startup 只跑一次"全靠它（已记《Bevy结构笔记》§5）；
- **注意**：重置自己动手（`*stopwatch = Stopwatch::default()`）；闭包系统同样可用。

选型一句话：**全局看得见的用 `Res`，只有自己用的用 `Local`**。Unity 类比：≈ DOTS system 的私有实例字段 / MonoBehaviour 成员变量。

## 5. 高频问答速查

| 问 | 答 |
|---|---|
| `Res<T>` 能直接传 `T`/`&T`/`mut T` 吗 | 都不行。`T` 不实现 SystemParam 且系统无权拥有 World 数据；`&T` 无此 impl（引用型只有 `&World`/`&Archetypes`/`&Components`/`&Entities`，system_param.rs:785-1473）；`mut` 不是类型。访问器 = 权限声明 + Deref + 缺失策略 |
| `Assets<T>` 的 T 固定吗 | 开放集合：任何 `#[derive(Asset)]` + `init_asset::<T>()`，自定义一等公民 |
| 为什么 `Res<Assets<T>>` 不直接 `Assets<T>` | `Assets<T>` 是 World 里的 Resource，系统只能借；可变语义由**类型**（`Res` vs `ResMut`）承载，不由 `mut` 承载 |
| `Query<T>` 和直接 `T` | 组件按实体存，裸 T 无意义（哪个实体的？）；Query = 集合访问器 + 过滤器 + 权限声明；单实体用 `Single` |
| `Res<T>` 是 `Single<T>` 吗 | 方向对，且 0.19 里同属 `SystemParamValidationError` 验证框架（Res 的 doc 原话 "fails validation"）。差异在担保来源：`Res` 唯一性=**存储结构担保**（类型即键、insert 覆盖、永不歧义），`Single` 唯一性=**数据状态校验**（每帧 O(n)、0 或 ≥2 都失败→跳过）；失败后果：Res 默认 panic（接线 bug 配硬失败），Single 跳过（状态波动配软失败） |
| Resource 为什么只能有一个 | 见《数据层全景》§3.2——API 键选型（类型即钥匙），多份出路：newtype / 资源内装集合 / 升格实体 |

## 相关阅读

- 《System与调度DSL.md》——参数如何被榨取、构图、编译、并行执行
- 《数据层全景：World、Entity、Resource与Asset.md》——参数访问的数据模型（World/Resource/Entity/Asset）
- 《Bevy结构笔记.md》——帧循环与 update() 内部
- 示例：`examples/ecs/system_param.rs`（derive 自定义参数）、`fallible_params.rs`（Single/Option）、`custom_query_param.rs`、`message.rs`、`observers.rs`、`3d/anisotropy.rs`（Local 实战）
