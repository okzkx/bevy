# Bevy 结构笔记（一）：App / SubApp / Schedule / World

> 2026-09-14。起因：跑 `examples/hello_world.rs` 后在源码里撞见四个概念。本篇按"谁包含谁 → 每个是什么 → 一帧怎么转起来"讲清，全部以本仓库源码为准（标注 file:line）。是《学习目标实现步骤》第一步四问中 Q1/Q3 的解答。

## 0. 全景：谁包含谁

```
App（组装器 + 生命周期管理，app.rs:85）
├─ runner: RunnerFn ──────────── 外部循环：谁来一遍遍调 update()
│                                （run_once / WinitPlugin / ScheduleRunnerPlugin）
└─ sub_apps: SubApps（sub_app.rs:564）
   ├─ main: SubApp ───────────── 你的游戏住这里
   │  └─ world: World ────────── 数据总仓库（sub_app.rs:67）
   │     ├─ 实体 + 组件
   │     ├─ 资源（Resources）
   │     └─ Schedules 资源 ───── 所有调度器本身也存放在 World 里！
   │        └─ Main 调度 ─────── 元调度，按 MainScheduleOrder 依次跑子调度
   └─ sub_apps: HashMap<标签, SubApp> ── 第二世界，如 RenderApp
      （每个 SubApp 自带独立 World + extract 函数，sub_app.rs:65）
```

**一句话**：App 是外壳和组装器；SubApp 是"半个引擎"（各带一个 World 和一套调度）；World 存一切数据；Schedule 是存在 World 里的执行计划，决定系统何时以什么顺序跑。

## 1. World —— 数据总仓库

> 深入版（字段解剖、Resource=隐形实体组件、变更检测 tick、Commands 延迟）见《[World与Resource](World与Resource.md)》。

- 装三样东西：**实体+组件**（ECS 的数据行）、**资源**（全局单例，如 `Time`、`Assets<Mesh>`）、以及——容易忽略的——**调度器集合 `Schedules` 也是一个存在 World 里的资源**（`sub_app.rs` 的 `SubApp::default()` 里就是 `world.init_resource::<Schedules>()`）。
- World 是**被动的**：它不跑逻辑，只被系统读写。"世界在动"其实是"挂在它上面的 Schedule 在跑"。
- Unity 类比：≈ 把"所有已加载场景的对象仓库 + 全局单例"合并成的一个数据容器。没有 Unity 那种多场景叠加概念——Bevy 的场景（glTF/BSN）本质是"往 World 里批量 spawn 的一组实体"。

## 2. Schedule —— 执行计划

- **是什么**：一个带标签的系统集合 + 排序规则（显式 `.before/.after` 约束 + 执行器自动并行）。`add_systems(Update, foo)` 的语义 = 把 `foo` 注册进标签为 `Update` 的那个 Schedule。
- **存在哪**：World 的 `Schedules` 资源里（见上）。
- **Main 是元调度**：`App::default()` 设 `main.update_schedule = Main`，`SubApp::update()` 就是跑 Main 调度。Main 自己不装系统，它按 `MainScheduleOrder`（资源，`bevy_app/src/main_schedule.rs:214`）依次执行子调度。
- **0.19.1 默认主循环顺序**（`main_schedule.rs` 的 `MainScheduleOrder::default()`，逐字核实）：
  ```
  每帧：First → PreUpdate → RunFixedMainLoop → Update → SpawnScene → PostUpdate → Last
  启动：PreStartup → Startup → PostStartup
  ```
  - `First`：消息/时间等基础维护（`message_update_system` 挂在这，`sub_app.rs` SubApp::default）；
  - `RunFixedMainLoop`：定点步循环的壳，`FixedMain` 在里面按固定频率跑（物理类逻辑）；
  - **`SpawnScene` 在 Update 之后、PostUpdate 之前**：glTF/BSN 场景 spawn 的延迟队列在这里应用——这解释了"spawn 场景后要过一两帧实体才齐"；
  - 插件可用 `insert_after/insert_before` 插队（如 bevy_state 把 `StateTransitions` 插进 PreUpdate 后）。**0.19.1 默认列表里没有 StateTransitions**，它是被 bevy_state 插进来的。
- **执行器自动并行**：同一 Schedule 内无数据冲突的系统自动多线程跑，冲突判定来自系统参数的访问签名（`Query<&mut T>` 与 `Query<&T>` 冲突等）。你要跨调度保序，用 `.after(bevy_transform::TransformSystems::TransformPropagate)` 这类链式约束（入口篇已记）。
- Unity 类比：≈ PlayerLoop 的阶段划分（Update/LateUpdate/…），但阶段在 Bevy 里是**可插拔的数据结构**，且阶段内自动并行。

## 3. App —— 组装器与生命周期

- **它小得惊人**，就三个字段（`app.rs:85`）：`sub_apps`、`runner`（生命周期函数）、`fallback_error_handler`。游戏数据一个字段都没有——全在 World 里。
- `add_plugins()` 做的事：调用每个 `Plugin::build(app)`，让插件往 World 插资源、往 Schedules 注册系统、再挂更多 SubApp。**App 自己不实现功能，它是插件们的装配台**。
- `run()` 把控制权交给 `runner`。`runner` 决定"每帧谁来调 `app.update()`"：
  - `App::empty()` 默认 `runner = run_once`（`app.rs:152`）——**只跑一帧就退出**；
  - **这就是 hello_world 只打印一行"hello world"就结束的原因**：它连 DefaultPlugins 都没加（全文件 6 行），默认 runner 是 run_once。平时感觉"Bevy 程序一直在转"，是因为 DefaultPlugins 里的 `WinitPlugin` 把 runner 换成了 winit 事件循环（每收到一次红raw事件/每帧调一次 update）；
  - 无窗口时用 `ScheduleRunnerPlugin::run_loop(1/60)` 手动给个循环（`examples/app/headless.rs` 的做法）。
- **runner 真身表**（2026-09-14 补核实）——有趣的事实：**DefaultPlugins 里其实有 `ScheduleRunnerPlugin` 条目**，只是带门控 `#[custom(cfg(not(feature = "bevy_window")))]`（`bevy_internal/src/default_plugins.rs:19-20`）：

  | 构建 | 生效的 runner | 说明 |
  |---|---|---|
  | 桌面（默认） | `WinitPlugin` 的 winit_runner | ScheduleRunnerPlugin 条目被剔除；循环在 OS 消息泵（`EventLoop::run_app` 各事件回调内调 `app.update()`） |
  | headless（禁 `bevy_window`） | `ScheduleRunnerPlugin`，默认 `RunMode::Loop { wait: None }`（`schedule_runner.rs:33`） | **全速空转、不 sleep**——headless 例程要自己 `run_loop(Duration)` 限拍 |
  | 裸 `App`（无插件） | `run_once` | 一帧退出（hello_world） |

  `set_runner` 是"**后来者赢**"：DefaultPlugins 里 ScheduleRunnerPlugin（第 19 行）先 build、WinitPlugin（第 40 行）后 build，后者覆盖前者。ScheduleRunner 的 loop 体就三件事（`schedule_runner.rs:97-125`）：`app.update()` → `should_exit` 检查 → 可选 `sleep(wait - 耗时)`；首循环前履行 `finish()`/`cleanup()`（Plugin 生命周期后两段归 runner）。
- **hello_world 逐行**：
  ```rust
  App::new()                          // App::default()：建 main SubApp + World + Main 调度
      .add_systems(Update, hello_world_system)  // 把函数注册进 Update 子调度
      .run();                         // 交给默认 runner（run_once）：finish→cleanup→update()×1→退出
  ```

## 4. SubApp —— 第二个"半个引擎"

- 每个字段都值得看（`sub_app.rs:65`）：
  | 字段 | 含义 |
  |---|---|
  | `world: World` | 独立的数据世界 |
  | `update_schedule` | 这个 SubApp 每帧跑哪个调度 |
  | **`extract: Option<ExtractFn>`** | **双 World 机制的落点**：源码注释原文——"gives mutable access to two app worlds…primarily intended for copying data from the main world to secondary worlds"（同时可变访问两个 World 的函数，用途就是把主 World 数据拷给副 World） |
- `SubApps::update()`：先跑 main，再逐个跑子 app；子 app 先经 `extract` 从主 World 拷数据，再跑自己的 `update_schedule`。
- **RenderApp 就是 bevy_render 挂的一个 SubApp**（标签 `RenderApp`）——之前几轮讨论的"官方双 World/Extract"在这里落地：主世界跑 Update（下一帧逻辑）的同时，RenderApp 拿着 extract 拷来的快照准备/提交上一帧。
- **我们的方案**：`disable::<RenderPlugin>()` → RenderApp 这个 SubApp 压根不会被创建 → 全项目只有一个 main SubApp 的 World → 渲染系统就是主世界 PostUpdate 里的普通 system。
- Unity 类比：≈ "另一套 player loop + 私有场景数据的合体"。RenderApp 相当于把 Unity 渲染线程的数据面做成了 ECS。

## 5. 串起来：一帧的时间线（主世界视角）

```
runner tick（winit 事件 / ScheduleRunner 定时 / run_once 单次）
└─ SubApps::update()
   ├─ main.update()：跑 Main 调度
   │   First → PreUpdate → RunFixedMainLoop → Update → SpawnScene → PostUpdate → Last
   │                                              ↑游戏逻辑      ↑场景spawn落地  ↑变换传播+渲染采集(我们)
   └─ （若有）各 SubApp：extract(主World→副World 拷贝) → 跑自己的调度（如 Render）
```

**`app.update()` 内部三层**（2026-09-14 补核实，每层都薄得意外）：

1. **`App::update()`**（`app.rs:161-166`）：一个 `is_building_plugins` panic 卫兵 + 一行 `self.sub_apps.update()`——完。
2. **`SubApps::update()`**（`sub_app.rs:574-591`）：①`main.run_default_schedule()`；②for 每个 SubApp：`extract(&mut main.world)` → `update()`；③`main.world.clear_trackers()`。
   - `SubApp::update()` = `run_default_schedule()` + `self.world.clear_trackers()`（`sub_app.rs:155-159`）；
   - `run_default_schedule()` = 跑 `update_schedule` 指向的调度——**SubApp 生下来 `update_schedule: None`，不设就静默空转**。
3. **Main 调度的真身**：不是特殊结构，就是一个 **SingleThreadedExecutor 的普通调度 + 唯一系统 `Main::run_main`**（`main_schedule.rs:313-315`）：

   ```rust
   pub fn run_main(world: &mut World, mut run_at_least_once: Local<bool>) {
       if !*run_at_least_once {
           for &label in &order.startup_labels {      // [PreStartup, Startup, PostStartup]
               world.try_run_schedule(label);
           }
           *run_at_least_once = true;
       }
       for &label in &order.labels {                  // 每帧七段
           world.try_run_schedule(label);
       }
   }
   ```

   两个此前悬着的疑问在此闭合：
   - **"Startup 只跑一次" = 一个 `Local<bool>` 标志**（首帧跑完 startup 三连后翻转），没有任何特殊机制；
   - **"调度顺序" = `MainScheduleOrder` 资源里的两个 Vec + for 循环**（`main_schedule.rs:214-233`）——`insert_after`/`insert_startup_before` 都是往 Vec 插位，bevy_state 插 `StateTransitions` 用的就是这个机制。
   - 变更检测的"每帧结算"发生在 `clear_trackers()`：`is_changed()` 的窗口是一帧，因为帧尾把 tracker 清了。

## 6. 与 Unity 的映射（速查）

| Bevy | Unity 近似物 | 关键差异 |
|---|---|---|
| App | PlayerLoop + 引擎装配 | App 是你代码里可持有的对象，可手动泵 `update()` |
| World | 活动对象仓库 + 全局单例 | 单 World 装一切；场景=批量 spawn 的实体 |
| Schedule | PlayerLoop 阶段（Update/LateUpdate/…） | 阶段是数据：可插拔、可自定义；阶段内自动并行 |
| SubApp | 另一套 player loop + 私有数据面 | RenderApp = 官方渲染的独立世界 |
| System | MonoBehaviour.Update | 纯函数 + 显式声明数据访问；不挂在对象上 |

## 7. Plugin 机制 → 已独立成篇

Plugin 的添加机制、生命周期四段、PluginGroup/`plugin_group!` 宏展开与 `set`/`disable` 链式调用，2026-09-14 从本篇拆出为同目录《[Plugin与PluginGroup机制](Plugin与PluginGroup机制.md)》。

## 8. 遗留与下一步

- 第一步四问进度：**Q1（启动到第一帧）✅ 本篇第 3、5 节（2026-09-14 增补 runner 真身表 + update() 内部三层）；Q3（system 时机/变换传播位置）✅ 本篇第 2 节**；Q2（禁 RenderPlugin 后剩什么）✅《DefaultPlugins分类.md》；Q4（glTF 链路）→ 消费端半篇见《BSN场景语法与Unity场景对比.md》，剩 glTF 加载链路。
- 待深挖（按需，不阻塞）：`RunFixedMainLoop`/`FixedMain` 的定点步细节（`FixedMain::run_fixed_main` 同款 Vec 遍历，`main_schedule.rs`）。~~extract 的具体调用时机~~ ✅ 2026-09-14 已核实（第 5 节 `SubApps::update` 三步）。
