# System 与调度 DSL：从 add_systems 到执行计划

> 2026-09-16。源起三问：① `add_systems` 的两个参数（`ScheduleLabel` / `IntoScheduleConfigs`）到底是什么；② Schedule 这块完全不懂——Unity 里"一个 System 一个类挨个 Update"，Bevy 是另一回事；③ 复杂传参（能传任何函数、还能按函数参数建图）是怎么做到的，对写 Rust 有什么启发。
> 本文吃透 **Bevy 调度体系全链**；Rust 语言层的可迁移套路单独记在《开发工具与语法笔记》§3。

## 1. add_systems 的四跳链

```rust
// crates/bevy_app/src/app.rs:321
pub fn add_systems<M>(
    &mut self,
    schedule: impl ScheduleLabel,                          // 参数1：装到哪
    systems: impl IntoScheduleConfigs<ScheduleSystem, M>,  // 参数2：装什么
) -> &mut Self
```

| 跳 | 位置 | 做什么 |
|---|---|---|
| 1 | `App::add_systems` (app.rs:321) | 转发给主 SubApp |
| 2 | `Schedules::add_systems` (schedule.rs:220) | `self.entry(schedule)`——**schedule 不存在就现场创建**（schedule.rs:136） |
| 3 | `Schedule::add_systems` (schedule.rs:430) | 一行：`graph.process_configs(systems.into_configs(), false)` |
| 4 | `process_configs` (schedule.rs:837) | 展开配置树 → 图节点 + 边 |

**记图立即，编译偷懒**：add 时只记节点和声明边；拓扑排序成执行计划是**首次运行**才做，图变了才重编译（类比着色器：add = 递给驱动，首帧 = link 成 PSO）。

## 2. ScheduleLabel：一把 intern 过的名字钥匙

`Update`、`FixedUpdate`、`Startup` 全是普通 unit struct + `#[derive(ScheduleLabel)]`。trait 本体由 `define_label!` 宏生成（set.rs:19，宏 label.rs:86）：

```rust
pub trait ScheduleLabel: Send + Sync + Debug + DynEq + DynHash {
    fn dyn_clone(&self) -> Box<dyn ScheduleLabel>;
    fn intern(&self) -> Interned<dyn ScheduleLabel>;  // 全局去重
}
```

- `Schedules` 本体 = `HashMap<InternedScheduleLabel, Schedule>`（schedule.rs:47）；
- `Interned` 是全局去重的 `Arc`——同一 label 类型全进程一份，之后比较/哈希/拷贝退化为**指针操作**。与 `Entity::to_bits` 同一思路：**类型身份压成可复制小值**；
- 集合开放：任何 crate 都能 derive 一个新 ScheduleLabel 造自己的调度器（RenderApp 的 Render 系列就是别处定义的）。

**Unity 类比**：ScheduleLabel ≈ PlayerLoop 子系统名（Update/FixedUpdate/LateUpdate），只是 Bevy 的集合开放、可自定义。

## 3. 终点先看清：System 是什么运行时对象

`add_systems` 收下的每个函数最终变成 `Box<dyn System<In=(), Out=()>>`（别名 `ScheduleSystem`，schedule_system.rs:199）。

`System` trait（system.rs:34）关键成员：

- `flags()`：三比特——`NON_SEND`（含 NonSend 不能跨线程）/ `EXCLUSIVE`（拿 `&mut World`）/ `DEFERRED`（有命令缓冲）（system.rs:14）；
- `unsafe fn run_unsafe(input, UnsafeWorldCell)`：真正执行入口。**unsafe 的原因：调用方必须保证无冲突访问**——这个保证正是调度器用 DAG 换来的，安全性责任在 schedule 不在系统；
- `initialize(&mut World) -> FilteredAccessSet`：登记本系统读写集；
- `apply_deferred` / `queue_deferred`：Commands 落账点。

函数变成的 `FunctionSystem`（function_system.rs:503）：

```rust
pub struct FunctionSystem<Marker, In, Out, F> {
    func: F,
    state: Option<FunctionSystemState<F::Param>>,  // 参数状态，初始 None
    system_meta: SystemMeta,                        // 名字+访问集+flags
    ...
}
```

**两段式构造**：编译期生成（函数→FunctionSystem），`state` 要等 `initialize(world)` 才建（Query 缓存等依赖 World 的东西）。类比：Unity DOTS 的 ISystem 是你手写的 struct；Bevy 的"struct"由**函数签名自动生成**，参数列表就是成员变量。

## 4. IntoScheduleConfigs：DSL 的全部秘密是一棵树

trait 只有一个必需方法（config.rs:314）：`into_configs() -> ScheduleConfigs<T>`，其余全是语法糖。

`ScheduleConfigs` 两叉枚举（config.rs:99）——**DSL 即配置树**：

```rust
pub enum ScheduleConfigs<T: Schedulable> {
    ScheduleConfig(ScheduleConfig<T>),   // 叶子：一个节点
    Configs {                            // 分支：一组子树
        configs: Vec<ScheduleConfigs<T>>,
        collective_conditions: Vec<BoxedCondition>,  // 组级条件，一帧评一次
        metadata: Chain,                 // 记录是否 .chain() 过
    },
}
```

叶子 `ScheduleConfig`（config.rs:92）= 节点 + `GraphInfo`（归属 set / before-after 边 / 歧义豁免）+ `Vec<BoxedCondition>`（条件）。

### 4.1 五条 impl 路径

| 你写的 | 走哪条 impl | 结果 |
|---|---|---|
| 裸函数 `a` | 函数 blanket impl（config.rs:561） | `IntoSystem` 装箱 → 叶子 |
| `BoxedSystem` | config.rs:571 | 直接叶子 |
| `SomeSystemSet` | config.rs:577 | set 配置节点（供 configure_sets） |
| 元组 `(a, b)` | 宏生成 1~20 元（config.rs:586-620） | `Configs { configs: vec![...] }` |
| 已配置树 `a.after(b)` | `impl for ScheduleConfigs`（config.rs:485） | 继续链式返回自身 |

嵌套由此成立：`(a, (b, c).after(a))` —— 每个元素各自转树，元组把子树装进一个分支。

### 4.2 配置方法的实际效果

| 方法 | 干什么 |
|---|---|
| `.in_set(S)` | 递归给每个叶子的 hierarchy push 一个 set |
| `.before(S)` / `.after(S)` | 递归给每个叶子 push 依赖边 |
| `.run_if(c)` 单个 | 进叶子 conditions |
| `.run_if(c)` 组 | 进 collective_conditions——**整组一帧只评一次**，等价挂临时 set |
| `.distributive_run_if(c)` | 条件**克隆**进每个叶子，各评各的 |
| `.chain()` | 只改组元数据标志位（config.rs:249）；相邻子树连边发生在 process_configs 展开时 |
| `.ambiguous_with(S)` | 记歧义豁免，压制冲突警告 |
| `*_ignore_deferred` 系列 | 出边上**不**自动插 ApplyDeferred |

条件也是系统：`new_condition`（config.rs:15）把条件函数 `into_system` 装箱成 `BoxedCondition = Box<dyn ReadOnlySystem<In=(), Out=bool>>`（condition.rs:11）——有自己的参数和状态。

### 4.3 匿名集：为什么 `.after(函数名)` 能用

裸函数走 `IntoSystemSet` 转换（set.rs:289）：包成 `SystemTypeSet<F>`（set.rs:189）——**用函数的类型当 set 身份**。每个系统的默认归属集里就有"自己这个类型"（`default_system_sets()`，config.rs:55 塞进 hierarchy），build 时按类型对上号。

### 4.4 元组 vs 多次 add_systems（判据一句话）

**配置打在元素上 = 与多次分开调用完全等价**：元组 impl（config.rs:586-620）把每个元素各自 `into_configs()` 装进 `Configs { metadata: Chain::Unchained（schedule.rs:282-285，#[default]）, collective_conditions: 空 }`，process_configs 对 Unchained 组**零连边**，各叶独立入图——`examples/3d/3d_shapes.rs:36-44` 三个 run_if 各异的系统就是这么罗列的。

**元组独有能力 = 配置打在括号上**（无"分开多次 add"的直接等价物）：`(…).run_if(c)` 组级条件一帧评一次全组同进退 / `.in_set` / `.chain()` 相邻连边 / `.distributive_run_if`。`#[cfg]` 写在元素上裁掉后元数自动减（all_tuples 覆盖 1~20）；超 20 嵌套元组。

判据：**打元素 = 等价，打括号 = 元组独有**。

## 5. 复杂传参的三层机制（精简版）

"能传任何函数、还能按参数建图" = 三个独立 Rust 机制叠加：

1. **函数是匿名 ZST 类型**。`impl IntoScheduleConfigs<...>` 是泛型形参，逐函数单态化——每个函数各得一份转换代码，零运行时开销，不是 `dyn` 分发；
2. **签名被"榨"成关联类型**。`SystemParamFunction<Marker>`（function_system.rs:853）的宏实现（:874）用 `for<'a> &'a mut F: FnMut(SystemParamItem<P1>, ...) -> Out` 高阶约束匹配调用形态，编译器由此唯一确定 `type Param = (P1, ..., Pn)`——**参数列表本身成为一个类型**，后续泛型代码随便引用。传了非 SystemParam 参数 → where 不满足 → `#[diagnostic::on_unimplemented]` 友好报错（config.rs:310）；
3. **参数递归登记访问集**。`Param` 元组本身也实现 SystemParam（还是宏生成的），`init_state`/`init_access`（system_param.rs:229-237）递归下钻：`Res`=读、`ResMut`/`&mut`=写、filter=原型读、`Commands`=DEFERRED 旗标。汇总成 `FilteredAccessSet`——**"根据参数建图"的参数面**。

分工：**参数提供数据面（冲突判定、同步点），手写 before/after 提供控制面（顺序边）**，合成完整 DAG。Marker 泛型（`IsFunctionSystem`/`ScheduleConfigTupleMarker`）防 impl 家族重叠 + 定位报错，见《开发工具与语法笔记》§3。

## 6. 建图 → 编译 → 执行

首帧前 `build_schedule`（schedule.rs:1155）五步：

1. set 层级 DAG 分析（查环、删冗余边）；
2. 依赖 DAG 分析（before/after 查环）；
3. **set 拍平**：set 上的顺序约束下推到每个具体系统；
4. build pass：`auto_insert_apply_deferred` 在有 Commands 的系统出边自动插 `ApplyDeferred` 同步点；
5. 歧义检测（访问冲突且无顺序 → 按配置警告/报错）→ **拓扑排序**拍成扁平 `SystemSchedule`（schedule.rs:1355）：数组+依赖索引，缓存友好。

执行：native 默认 `MultiThreadedExecutor`（executor/mod.rs:49），每帧算**就绪集**——依赖全部完成且访问不冲突的系统同时跑；`Main` schedule 本身用 SingleThreaded（main_schedule.rs:313-315，它只有一个系统，没得并行）。

## 7. Schedule 心智模型（校验版）

> **修正 Unity 直觉**：Unity DOTS 其实有几乎同构的东西——真正的分野不是"类 vs 函数"，而是**顺序和并行从哪来**。

```
一帧 (app.update())
│
├─ Main schedule（只含 1 个系统 Main::run_main，单线程）   main_schedule.rs:290
│    └─ run_main 循环 try_run_schedule(label)：
│         第 1 帧: PreStartup → Startup → PostStartup
│         每帧:    First → PreUpdate → RunFixedMainLoop
│                  → Update → SpawnScene → PostUpdate → Last
│
└─ 每个标签 = 查 Schedules 字典 = 拿到 Schedule 实例：
     ┌─────────────────────────────────────────────┐
     │ Schedule(label = Update)                     │
     │  ① graph      节点+边（add_systems 攒的）    │
     │  ② executable 首帧编译的扁平执行计划          │
     │  ③ executor   跑法（native 默认多线程）       │
     └─────────────────────────────────────────────┘
```

四个角色别混：`Schedules`=字典；`Schedule`=一个帧阶段的容器（图+计划+执行器三件套）；`ScheduleLabel`=钥匙；`Executor`=跑法。**Schedule 本身不执行逻辑**，"挨个 Update"发生在 executor 层。

### Unity DOTS 对照

| Unity DOTS | Bevy | 同构？ |
|---|---|---|
| PlayerLoop 子系统 | Schedule | ✅ |
| ComponentSystemGroup | Schedule 内部结构 | ✅ |
| ISystem/SystemBase 类 | FunctionSystem（由函数自动生成） | ✅ |
| `[UpdateInGroup]` `[UpdateBefore/After]` | `.in_set()` `.before()` `.after()` | ✅ |
| 组内顺序执行，并行手写 Job+Burst | **executor 自动并行** | ❌ 真正分野 |

一句话：**Bevy 把 Job 层的并行提升到 System 层**——写函数签名的代价，换来原本手写 Job 才有的并行。

### 并行规则

**能并行 ⇔ 无顺序边 + 访问集不冲突**：

```rust
// 例1 并行✓：读 Time+写 Transform vs 只读 Score，互不相干
fn move_bodies(time: Res<Time>, mut q: Query<&mut Transform, With<Velocity>>) {}
fn update_score_ui(score: Res<GameScore>) {}

// 例2 串行✗：都写 Health，冲突；未声明顺序 → 歧义警告，顺序不定
fn apply_damage(mut q: Query<&mut Health>) {}
fn check_death (mut q: Query<&mut Health>) {}

// 例3 串行✗但确定：声明了 after，先 b 后 a
fn a(mut q: Query<&mut Health>) {}
fn b(mut q: Query<&mut Health>) {}   // a.after(b)
```

两种硬屏障（全体等它一个）：① exclusive 系统（拿 `&mut World`，跟谁都冲突）；② `ApplyDeferred` 落账点（Commands 先生效，下游才看得见）。**"挨个 Update"只发生在冲突子集上**，其余每帧并行铺开。不按注册顺序跑的根本原因：注册顺序在并行世界无意义——无冲突系统强行排序是浪费，冲突系统调度器不敢猜（猜错=数据竞争），只能让你声明。

**与 SubApp 的关系**：以上并行都在**单个 Schedule 内**；RenderApp 的渲染在独立线程，是 SubApp 级并行（见《SubApp机制与取舍》§7），两重并行别混。

## 8. 使用层面的坑（源码注释明确警告，config.rs:348）

- `(A).after(B)` **不会**把 B 自动加进 schedule——B 没被显式 add 时边悬空，两者乱序跑；
- 跨 schedule 的 before/after/chain **静默忽略**（Update 里的系统 after FixedUpdate 里的系统 = 没说）；
- 元组顺序 ≠ 运行顺序（除非 `.chain()`）——别依赖书写顺序。

## 9. 对自研渲染器的落点

"参数即能力声明 → 编译期推导调度"可直接迁移到 bindless 渲染器的资源管理：设计 `Read<Texture>` / `Write<Texture>` 之类的参数类型收集每个 pass 的读写集，**自动推导 pipeline barrier**（写后读=布局转换、写后写=barrier）——正是 Bevy 对 Commands 自动插 ApplyDeferred 的翻版。Vulkan 手写 barrier 最易漏同步，让签名推导是根治手段（详见《开发工具与语法笔记》§3 启发 3）。

## 相关阅读

- 《System参数与数据访问.md》——参数全量目录、Commands 同步点、Local（本文的参数侧姊妹篇）
- 《数据层全景：World、Entity、Resource与Asset.md》——参数访问的数据模型
- 《Bevy结构笔记.md》——帧循环时序、runner、update() 三层
- 《SubApp机制与取舍.md》——SubApp 级并行与渲染线程
- 《开发工具与语法笔记.md》§3——本文机制的可迁移 Rust 套路
- 源码：`bevy_ecs/src/schedule/config.rs`（DSL 本体）、`schedule.rs`（图与编译）、`system/function_system.rs`（函数→System）、`system/system_param.rs`（参数→访问集）
