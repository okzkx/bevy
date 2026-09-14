# SubApp 机制与取舍

> 2026-09-14。两轮问答整理，是《Bevy结构笔记》第 4 节（SubApp）的展开。全部结论以本仓库源码为准，标注 file:line。

## 1. 定位：官方设计意图，与"渲染是唯一用户"的现实

`SubApp` 的源码文档注释（`crates/bevy_app/src/sub_app.rs` 顶部）：

> "A secondary application with its own World. These can run independently of each other. These are useful for situations where certain processes (e.g. a render thread) need to be kept separate from the main application."

机制本质 = **独立 World + 独立 update_schedule + 一条 extract 单向通道**（`type ExtractFn = Box<dyn FnMut(&mut World, &mut World) + Send>`，同时可变访问两个世界，用途就是把主世界数据拷给副世界）。

**本仓库 0.19.1 事实**：引擎内实际创建的 SubApp 只有两个——`main` 和 `RenderApp`。其余几十个 `sub_app_mut` 的使用者（bevy_pbr、bevy_core_pipeline、bevy_anti_alias、bevy_solari……）全是**往 RenderApp 里注册系统**，不是创建新 SubApp。所以"官方用它跑渲染"不是主要用途，是唯一内置用途。

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
