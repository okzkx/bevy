# Bevy World 与场景生命周期：闯关全清、场景图与引用计数

> 建档（2026-09-29）：由两轮问答沉淀——"为什么 Vulkan 对象放 ECS World 而不是静态变量 / 对比 Unity 删 World 再开"与"闯关游戏每关全清场景内存怎么个做法"。API 均在仓库 0.20.0-dev 内核实（`DespawnOnExit` 见 `crates/bevy_state/src/state_scoped.rs:145`）。
> 关联篇目：《场景三层含义：glTF scene、WorldAsset与引擎World》（3.1 文件夹）、[图形管线机制篇](../3-静态取数链路/3.4-管线与绘制/图形管线机制：打包清单、双集ABI与bindless下的角色.md)（渲染器作为系统的另一面）。

## §0 判定线

1. **World 是引擎全量数据库，不是业务层**：Time、Window、Assets、输入都住在 World；Resource 是引擎服务的槽位，官方渲染器的 pipelines/bind group 缓存同样是 Resource。
2. **闯关全清 ≠ 删 World**：清的是"场景内容层"——状态机 + scoped 实体清理 + StateScoped 消息 + 强柄掉零 + 系统私有状态复位，五层各管一段；World 里的引擎单例（Time/Window/AssetServer 缓存）不清，是特性不是泄漏。
3. **场景结构是森林/图，不是单根树**：内容边界靠根实体/标记画，"进关挂同根"是编排习惯；0.16 关系化后 `ChildOf` 可多父，despawn 连带只覆盖唯一父链。
4. **资产生命周期 = 强柄确定性掉零**：无 GC 无扫描，最后一份强柄 Drop 当场释放，资产家族传递退场；但它只管 `Assets` 容器——**GPU 驻留是第三层账**，要账本逆操作（自研渲染器的 retire 契约）。
5. **渲染器状态住 World 是"渲染器作为系统"的必然**：Res/ResMut 是调度器的借用检查；解耦的完整形态是 RenderApp SubApp，本项目 M2 是其退化版，`CollectedScene` 快照已是 Extract 形态。

## §1 World 的定位：引擎数据库而非业务层

bevy 的 World 装着引擎级单例（Time、Windows、`Assets<T>`、Input），Resource 是命名服务的槽位——"Vulkan 对象放 World"不是把渲染塞进业务，而是本项目一个架构决定的推论：**渲染器不是一个独立循环，而是一组 bevy 系统**（`draw_frame` 挂 `Last`），系统状态必须是调度器能声明、能守卫的东西。

**静态变量输在四处**：

| 维度 | ECS Resource | 静态变量 |
|---|---|---|
| 别名控制 | Res/ResMut 是调度器的借用检查，冲突系统调度期拒绝 | Mutex/RwLock 或 unsafe，散落各处 |
| 拆除序 | 插入序 = 创建序 = 依赖序，`remove_resource` 显式反序拆除（Drop 序 = 对象依赖序） | 进程退出 Drop 顺序未指定（常不跑），与 hwnd 销毁序拼成 UB 面 |
| 失败语义 | 全有或全无：失败不插入，`run_if(resource_exists)` 整体跳过 | 自造 Option + 已初始化标志，守卫散落 |
| 实例性 | `#[derive(Resource)]` 只是 marker——类型仍是普通 Rust 值，探针在无 World 处建过两套完整栈 | 进程单例，不可二建、不可传值 |

最后一行容易被误读，值得钉死：**Resource 化不等于绑死 World**。真正的绑定发生在 `insert_resource`，不在类型上。

## §2 渲染状态住 World：取舍与 Unity 对照

M2 形态 = 单 World、帧末直读、vulkan 资源与业务同库。代价是渲染状态与主 World 同寿——"删 World 换一局"没有发生点（bevy 主 World 本就是进程级，场景是作为实体加载进同一 World，不换 World）。完整解耦形态是 **RenderApp SubApp**（官方渲染器自己住第二个 World，业务每帧 Extract 进去）；本项目接口已埋好：`CollectedScene` 快照只带拓扑不带内容，形态即 Extract 产物，将来 GPU-driven/多窗口/换 World 时把 vulkan 资源挪进 SubApp 或 runner 级容器，边界现成。

Unity 对照：DOTS 能整 World 扔，不是 Unity ECS 更强，而是**它的渲染状态根本不在 World 里**——SRP 由 player loop 驱动、自持状态，World 删了重建不碰渲染。bevy 官方渲染器的对应物是 SubApp，不是静态。

## §3 闯关全清：五层配方

**① 状态机层**：`#[derive(States)]` 定义 `Gameplay(关卡号)`，自动获得 `OnEnter/OnExit/OnTransition` 调度点；切关 = 写状态资源，切换发生在帧边界（StateTransition），不会帧中间半清。

**② 实体层**：两条路。轻量路——关卡生成的实体挂 `DespawnOnExit::<GameLevel>(当前关)`（组件见 `crates/bevy_state/src/state_scoped.rs:145`），状态退出时 `enable_state_scoped_entities` 启用的清理系统按标记连子孙 despawn；整关重货（glTF）就一个根：`AssetServer.load` → spawn 成根实体（WorldAssetRoot 形状）→ `OnExit` 时根实体一个 despawn 连带整棵树。

**③ 消息层**：关内事件用 `StateScopedMessages`，状态切换时未读消息自动清空——否则上一关的"玩家死亡"漏进下一关的 Update。

**④ 资产层**：强柄掉零自动释放（§5），主动掀桌才 `assets.clear()`。AssetServer 的按路径缓存不清——闯关不重复读盘靠它。

**⑤ 系统私有状态**：`Local<T>` 跨帧跨关存活（宿主里的 `ResizeGate`/`DrawListState`）——关卡相关的要么放 State-scoped resource，要么 `OnExit` 手动复位，否则"上一关 boss 已死"标记活到下一关。

## §4 场景图的精确形状

- **森林而非单根树**：实体可以无父散在表里，同一时刻多棵独立的树（关卡根/玩家/UI）并存。"进关挂同一个根"是**编排约定**（退场 = 一个 despawn），不是引擎结构；`DespawnOnExit` 更是标记即边界，不依赖树。
- **0.16 起 `ChildOf` 是普通关系组件，可多父**："场景树"实为"场景图"。连带语义：despawn 一个父只连带**唯一父是它**的子孙，多父实体不连带——UI 嵌套、双 parenting（空间父 + 交互父）时是行为差异不是 bug。
- **WorldAssetRoot = "一个资产实例"的根**：FlightHelmet 展开出的那棵树是资产实例形状；关卡根是编排层自己 spawn 的另一个根，两者平级。

## §5 引用计数的精确形状

- **确定性掉零**：`Assets<T>` 内部 `HashMap<AssetId, (强计数, 资产)>`，最后一份强柄 Drop 当场从容器移除——不是 GC、无扫描，释放点就是那个 Drop。
- **与实体的自动咬合**：柄住在组件里——despawn 连带组件 Drop、柄 Drop、计数减一，"清实体就清了资产"是自动的，游戏代码没有 release 调用。
- **资产家族传递退场**：glTF 场景资产持有全部子资产（mesh/贴图）的强柄；丢场景根柄 → 场景资产释放 → 子资产跟着掉零（前提是引用它们的实体也已 despawn）。
- **等价去重**：同路径重复 `load` 命中 AssetServer 缓存，复用同一资产与 id，不读两遍盘。
- **边界：CPU 释放 ≠ GPU 释放**。自研渲染器的 MeshPool bump 区间、ImageCache、描述符槽都是按 AssetId 登记的驻留账本——引用计数掉零后 GPU 驻留纹丝不动。闯关全清的完整形状 = 引用计数清 CPU + **retire 按票据清 GPU**（三条安全契约已立在 `BindlessTables::retire_*`：最后使用票据等过、共享方全部退场、同槽再发布无交叠；M2 不调用，调用是步骤 4"运行时淘汰"的验收）。push 常驻表的好处在此显形：关卡换血只动数据槽位，管线与 set 绑定纹丝不动。

## §6 钉子汇总

1. Resource 是 marker trait，不绑 World——探针证明这些类型可以在无 ECS 处完整使用。
2. 主 World 无"删了再开"的发生点；唯一全清路径是退出，退出链被 `OnAppExitSystems` 里的反序拆除抢在 despawn_windows 之前。
3. 多父 `ChildOf` 下 despawn 连带语义按唯一父判定。
4. 引用计数管 `Assets` 容器；AssetServer 路径缓存、GPU 驻留是它管不到的两层，各需自己的清理账（前者保留，后者 retire）。
