# SubApp 机制与取舍

> 2026-09-14。两轮问答整理，是《Bevy结构笔记》第 4 节（SubApp）的展开。全部结论以本仓库源码为准，标注 file:line。

## 1. 定位：官方设计意图，与"渲染是唯一用户"的现实

`SubApp` 的源码文档注释（`crates/bevy_app/src/sub_app.rs` 顶部）：

> "A secondary application with its own World. These can run independently of each other. These are useful for situations where certain processes (e.g. a render thread) need to be kept separate from the main application."

机制本质 = **独立 World + 独立 update_schedule + 一条 extract 单向通道**（`type ExtractFn = Box<dyn FnMut(&mut World, &mut World) + Send>`，同时可变访问两个世界，用途就是把主世界数据拷给副世界）。

**本仓库 0.19.1 事实**（2026-09-14 修正，见 §7）：桌面默认 DefaultPlugins 实际创建 **3 个** SubApp——`main`、`RenderApp`、`RenderExtractApp`（最后一个来自 PipelinedRenderingPlugin）；headless / 单线程构建则只有 `main`（禁渲染）或 `main + RenderApp`。其余几十个 `sub_app_mut` 的使用者（bevy_pbr、bevy_core_pipeline、bevy_anti_alias、bevy_solari……）全是**往 RenderApp 里注册系统**，不是创建新 SubApp。所以"官方用它跑渲染"不是主要用途，是唯一内置用途。

API 面（`crates/bevy_app/src/app.rs:1220-1265`）：`sub_app` / `sub_app_mut` / `get_sub_app` / `insert_sub_app` / `remove_sub_app` / `update_sub_app_by_label`——为自建 SubApp 准备的完整入口；`sub_app.rs` 的 doctest 就是一个自定义 `#[derive(AppLabel)] struct ExampleApp;` 的最小示例。

### 一般还能用来做什么

- **编辑器/工具宿主**：进程内"工具世界 + 被编辑的游戏世界"各一个 SubApp，强隔离、互不污染；
- **同进程多实例**：headless 逻辑模拟 + 客户端表现、A/B 世界对比、回放/预测模拟；
- 理论上音频 mixer、物理预测等也能用，但社区没有第二个大规模用户——这类需求用普通系统 + 资源就够了。

### 什么时候值得开 SubApp（判断标准，三条同时满足）

1. 需要**独立的 World**（数据模型不同构 / 强隔离）；
2. 需要与主循环**不同的节奏或并行性**；
3. 数据流是"主世界 → 我"的**受控快照**（extract），而非双向共享读写。

渲染三条全中。默认永远先用普通系统。

## 2. 机制解剖（sub_app.rs:65）

| 字段 | 含义 |
|---|---|
| `world: World` | 独立数据世界 |
| `plugin_registry` / `plugin_names` / `plugins_state` | 自己的插件体系 |
| `update_schedule: Option<InternedScheduleLabel>` | 每帧跑哪个调度；**忘设 = 静默不跑** |
| `extract: Option<ExtractFn>` | 双 World 拷贝通道；**忘设 = 静默不拷** |

`SubApp::default()` 白送的部分（`sub_app.rs`）：`World::new()` + 初始化 `Schedules` 资源 + 把 `message_update_system` 挂进 `First`。

**每帧时序**（`SubApps::update()`，`sub_app.rs:575-588`，单线程顺序执行）：

```
main.run_default_schedule()          ← 主世界先跑完
for 每个 sub_app:
    sub_app.extract(&mut main.world) ← 主世界此刻必须全停写（硬同步点）
    sub_app.update()                 ← 跑自己的调度
main.world.clear_trackers()          ← 变更标记在 extract 之后才清
```

extract 能用变更检测（`Changed`），完全依赖"先 extract、后 clear_trackers"这个顺序；手动调 `update_sub_app_by_label` 时该时序归自己管。

## 3. 并行性的真相：SubApp 不给并行，PipelinedRenderingPlugin 才给

- **`SubApps::update()` 是单线程顺序的**——SubApp 机制本身零并行。
- 真正的"渲染与逻辑并行"来自 `PipelinedRenderingPlugin`（`crates/bevy_render/src/pipelined_rendering.rs:109`，默认在 DefaultPlugins，`default_plugins.rs:57`）：
  - 机制：把**整个 RenderApp（连 World）通过 channel 发到独立渲染线程**——`RenderAppChannels` 里就是 `Sender<SubApp>` / `Receiver<SubApp>`（World 按值移动是浅移动，arena 结构搬家，不拷数据，开销小）；
  - 代价：**一帧延迟**（渲染线程消化第 N 帧时主线程跑第 N+1 帧）；
  - 官方还专门留了 `RenderExtractApp` 子 app（`pipelined_rendering.rs:19`）处理"必须留在主线程的那部分"（extract 之后的时序/帧节奏）——复杂度可见一斑。

## 4. 开销账

| 项 | 量级 | 说明 |
|---|---|---|
| 构建（一次性） | 可忽略 | 第二个 World + 一套 Schedules，毫秒级 |
| 内存镜像 | ∝ extract 量 | 组件副本 + GPU 资源包装结构；World 骨架本身很小 |
| 每帧 extract | ∝ 变更量（好）/ 实体数（差） | `Changed` 过滤则便宜；全量 clone 则是分配大户 |
| 同步点 | 每帧一次 | extract 签名决定：提取瞬间主世界全停写 |
| 并行 | 非白送 | 要上 PipelinedRenderingPlugin，付一帧延迟 |

## 5. 风险账

1. **双份状态维护**（持续性维护税）：同一逻辑数据两个 World 各一份，删除/资产卸载靠 `AssetEvent`/`RemovedComponents` 手动同步，漏了 = 泄漏或悬空——bevy_render 的 extract 代码量巨大的根源；
2. **陈旧性**：子世界永远是快照，反馈路径再叠一帧；所有"这帧怎么没生效"的 bug 都长在这；
3. **静默失败**：`update_schedule`/`extract` 忘设均无报错，只有"没发生"；
4. **调试复杂度**：断点/日志/错误处理要分世界，panic 跨世界传播更绕。

## 6. 对本项目的结论

- **M5 之前不碰 SubApp**：这些开销与风险正是当初禁 RenderPlugin 省下来的全部；单 World 直读 = 零镜像、零 extract、零同步点，代价只是帧尾串行采集。
- **演进路径（需要时）**：若帧尾采集真的与游戏逻辑撞车（先用耗时数据确认，别猜）——自建 SubApp + `Changed` 驱动 extract + 参照 `PipelinedRenderingPlugin` 的 channel 模式挪线程。本质是"付镜像维护税 + 一帧延迟，买逻辑与准备的并行"。
- 官方两份对照材料：`examples/app/headless_renderer.rs`（extract + 主↔渲染通道的完整示范）、`crates/bevy_app/src/sub_app.rs` 的 doctest（自建 SubApp 最小示例）。

## 7. 补遗：3d_scene 的三个 SubApp、谁创建、运行期归属（2026-09-14 增补）

**创建者有三个，全在 DefaultPlugins build 期间**：

| SubApp | 直接创建者 | 触发链 | update_schedule | extract |
|---|---|---|---|---|
| `main` | `App::empty()` 直接构造（`app.rs:149`） | `App::new()` 必有；`app.rs:111` 设 `Some(Main)` | Main | None（被提取方） |
| `RenderApp` | **ExtractPlugin::build**（`extract_plugin.rs:36,75`） | RenderPlugin::build → `add_plugins(ExtractPlugin)`（`bevy_render/src/lib.rs:362`）→ RenderPlugin 里那句 `get_sub_app_mut(RenderApp)` 之所以能成功，就是它刚造的 | RenderRecovery（0.19：渲染调度包在 `run_if(renderer_is_ready)` 里） | `entity_sync + extract` |
| `RenderExtractApp` | **PipelinedRenderingPlugin::build**（`pipelined_rendering.rs:119-124`） | DefaultPlugins 条目带 `#[custom(cfg(not(wasm32), feature = "multi_threaded"))]`（`default_plugins.rs:56-58`），桌面默认满足 | **None**（文档："默认为空"） | `renderer_extract` |

关键底座事实：`SubApp::default()` = `update_schedule: None, extract: None`（`sub_app.rs:105-112`）——**SubApp 生下来什么都不跑，全靠后配置**；连 main 的"跑 Main 调度"都是 `App::new()` 一行赋值，不是天性。

**运行期归属转折**（`PipelinedRenderingPlugin::cleanup`，`pipelined_rendering.rs:135-178`——cleanup 由 runner 首循环前调用）：

1. `app.remove_sub_app(RenderApp)`——RenderApp 被**移出 SubApps 集合**；
2. 整个 SubApp（连 World）经 async_channel `send_blocking` 发给新建的 `std::thread`（渲染线程）；
3. 渲染线程死循环：收 RenderApp → `render_app.update()`（跑 Render 调度）→ 发回主线程。

所以"一共几个"分两个时态：**创建期 3 个；运行期 `SubApps::update` 每帧只遍历 main + RenderExtractApp 两个**。RenderExtractApp 的调度为空（空调转），每帧真正干活的只有 extract——`renderer_extract` 等 RenderApp 从渲染线程回来 → 跑 extract → 发回去。流水线 = 主线程跑第 N+1 帧 Update 时渲染线程还在渲第 N 帧。

**对 M1 宿主壳的直接推论**：`disable::<RenderPlugin>()` → RenderPlugin 不 build → ExtractPlugin 不挂 → RenderApp 不创建；PipelinedRenderingPlugin 有守卫 `if app.get_sub_app(RenderApp).is_none() { return; }`（`pipelined_rendering.rs:121`）→ 也不创建。**宿主壳运行期只有 1 个 SubApp（main）**，`SubApps::update` 的 for 循环遍历空 HashMap——每帧就是纯七段调度。§3 的"并行性靠 PipelinedRenderingPlugin"至此有完整源码落点：SubApp 只是数据结构（World + update_schedule + extract），并行是 cleanup 里那个 `std::thread` + 两条 channel 做出来的。
