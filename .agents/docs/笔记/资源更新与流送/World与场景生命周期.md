# Bevy World 与场景生命周期：换关清场清什么、不清什么

一个闯关游戏要切下一关：上一关的实体、界面、音效引用、Boss 死亡标记，都想在进新关前释放干净；同时不想把 `Time`、`AssetServer`（资产加载与路径解析服务）、渲染器上下文这些引擎级东西一起拆掉。另一个常被一起问的问题是：ash_renderer 里的 Vulkan 对象（设备、交换链、描述符表、网格池）为什么做成 ECS（实体组件系统）Resource，而不是进程静态变量？

**Bevy World 是进程级引擎数据库；换关清的是“场景内容层”（状态机、实体、消息、资产强柄、系统私有状态，五层各管一段），不是删 World；资产引用计数掉零只清 CPU 容器，GPU 驻留是另一层账，要按票据 retire。**

这里的“数据库”指 World 同时装着实体表、组件和全局资源，并且跟 App 同寿；“场景内容层”指随关卡生成、也随关卡退场的那部分东西，区别于 `Time`、`AssetServer`、渲染器上下文这些引擎级单例。强柄是持有资产引用、阻止资产被卸载的句柄；掉零指最后一份强柄被 Drop。票据（ticket）是 CPU 用来确认某批上传或提交已完成的值；retire 指把资源交回待回收账，等 GPU 不再读之后再复用；GPU 驻留指显存里的网格区间、图像和描述符槽。读完这一篇，你应该能列出换关清理清单，判断一个 Vulkan 对象该不该 Resource 化，并知道一次 despawn（从 World 删除实体）之后 CPU 资产和 GPU 槽位各自还要经过哪些帧边界才真正释放。

## 一、World 是进程级数据库，渲染器是挂在它上面的一组系统

Bevy 的 World 是实体、组件和全局资源的容器。`Time`、`Assets<T>`、`ButtonInput<T>` 这类全局值以 Resource（World 里按类型唯一存放的值）的形式住在里面；窗口不是 Resource，而是带 `Window` 组件的实体（`PrimaryWindow` 标记主窗口）；相机、灯光、模型实例也都是实体。整个 App 运行期只有一个主 World，场景是作为实体加载进这个 World 的，没有“删掉主 World 再开下一局”的引擎 API。

ash_renderer 没有另起渲染循环；它是挂在这个 World 调度上的一组系统：

- `draw_frame` 挂在 `Last`（帧末调度阶段），负责等 fence（CPU 用来确认 GPU 已执行完某次提交的同步对象）、acquire（取下一张交换链图像）、组装 DrawList（本帧可绘行列表）、录制提交；
- `flush_uploads` 也在 `Last`，排在 `draw_frame` 之前，负责把账本脏行上传到 GPU；
- `teardown_vulkan` 挂在 `OnAppExitSystems`，在窗口销毁前按创建表的反序拆除。

因此设备、交换链、描述符表、网格池这些 Vulkan 对象必须是调度器能声明、能守卫的东西：系统签名写 `Res<Context>`、`ResMut<MeshPool>`，调度器据此做借用检查；初始化失败时资源没插入，`draw_frame.run_if(resource_exists::<Context>)` 让整条帧循环跳过。退出时也是同一套编排：`ExitSystems` 先决定退出，`OnAppExitSystems` 里做拆除，`despawn_windows` 最后销毁窗口。

Resource 与进程静态变量的差别落在四个维度：

| 维度 | ECS Resource | 进程静态变量 |
|---|---|---|
| 别名控制（同一份数据的可变借用） | `Res`/`ResMut` 由调度器做借用检查：同一系统里 `Res` 与 `ResMut` 冲突会报错；不同系统间的 `ResMut` 冲突会被串行化 | 自己用 `Mutex`/`RwLock` 或 `unsafe`，访问点分散 |
| 拆除序 | 有显式拆除点：`teardown_vulkan` 在 `OnAppExitSystems` 里按 `init_chain!` 表逐个 `remove_resource`，规则是依赖者先拆、`Context` 最后 | 进程退出时析构顺序不由你定，窗口/设备这些外部资源可能已经先销毁 |
| 失败语义 | 创建链全成功才 `insert_all`，任一步失败一个都不插入；依赖系统用 `run_if(resource_exists)` 整体跳过 | 自造 `Option` 和初始化标志，守卫散落 |
| 实例性 | 类型是普通 Rust 值，`#[derive(Resource)]` 只加标记；同一类型在不同 World 各有一份 | 进程内唯一，不能一进程两套 |

`Resource` 本身是 marker trait（标记 trait，只声明类型可以被 World 按类型存放，不带行为；定义为 `pub trait Resource: Component {}`），`#[derive(Resource)]` 不会给类型加 World 句柄，也不会让类型只能在 World 里构造。真正的绑定发生在 `world.insert_resource(value)`：值被移进某个 World 的类型槽位，之后由这个 World 负责它的生命周期。类型本身仍可以单独构造，也可以插进另一个 World；所以“做成 Resource”和“绑死主 World”是两件事。

## 二、单 World 的取舍与完整解耦形态

ash_renderer 当前是单 World 形态：`init_vulkan` 在 `Startup` 建好 Vulkan 资源后 `insert_resource` 进主 World，业务实体、相机、灯光和 `Context`、`MeshPool`、`BindlessTextureTable` 同库；`draw_frame` 在帧末直接读主 World 的账本和资源。好处是路径短：渲染系统要什么就 `Res<...>`，不需要跨 World 拷贝；代价是渲染状态与主 World 同寿——主 World 本来就不换，所以“删 World 换一局”在这里没有发生点。

Bevy 官方渲染器走另一条路：主 App 挂一个渲染 SubApp（SubApp 是一个 App 内独立的第二套 World 和调度），每帧在 Extract（提取）阶段把主 World 里要画的数据拷进渲染 World，GPU 资源全住渲染 World。业务侧换关、删实体，不会直接碰渲染 World 的资源账。

本项目没有建自己的渲染 SubApp；宿主壳日志明确写着“无 RenderApp”。不过它已经把“场景数据”和“Vulkan 资源”分开了一半：`InstanceLedger` 是按 Entity 索引的跨帧账本（`BTreeMap<Entity, InstanceRow>`），一行只有两个资产柄、传播后的模型矩阵、对象表槽号和绑定身份；Vulkan 足迹（网格池区间、贴图槽）不进行，按 `AssetId` 去 `MeshPool`/`ImageCache` 现查。账本扮演的角色接近官方渲染器里 Extract 产物那一侧：只携带绘制需要的 ECS 事实，不携带 Vulkan 句柄。要彻底解耦时，对应做法是把 Vulkan 资源挪进 SubApp，账本这一侧不需要改语义。

Unity 的 DOTS（面向数据的 ECS 技术栈）能整个 World 创建和销毁，对照点不在 ECS 更强，而在渲染状态根本不在 ECS World 里：SRP（可编程渲染管线）由 player loop 驱动，管线对象自己持有渲染状态。Bevy 官方渲染器把这个角色交给 SubApp，而不是静态变量或主 World 里的业务组件。

## 三、换关全清：五层配方

### 1. 状态机层：把“第几关”做成状态

```rust
#[derive(States, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum GameLevel {
    #[default]
    Level1,
    Level2,
}

app.init_state::<GameLevel>();
// 切关系统里：next.set(GameLevel::Level2);
```

`StateTransition` 调度默认在 `PreStartup` 之前跑一次，之后每帧排在 `PreUpdate` 之后；`OnEnter`/`OnExit`/`OnTransition` 和 scoped 实体的清理系统都在这个调度里。玩法系统在 `Update` 里写 `NextState`，默认调度下切换不会在当帧中途发生，而是在下一次 `StateTransition` 点整体处理：依次跑 `DependentTransitions → ExitSchedules → TransitionSchedules → EnterSchedules`。退出阶段排在进入阶段之前；不过 `DespawnOnExit` 发的是命令，命令在同步点才应用，别把“新关 `OnEnter` 里旧实体已经全没了”当成必然。

### 2. 实体层：标记组件负责边界，根实体负责重货

轻量的关卡实体挂 `DespawnOnExit(GameLevel::Level1)`；同族还有 `DespawnOnEnter(GameLevel::Level2)`、`DespawnWhen::new(|transition| ...)`，以及只禁用/启用的 `DisableOnExit`/`EnableOnExit` 系列（`crates/bevy_state/src/state_scoped.rs`）。

`DespawnOnExit` 的清理系统不是挂在某个变体的 `OnExit` 上，而是挂在 `StateTransition` 的 `ExitSchedules` 里，查询带 `Allow<Disabled>`，用 `despawn_all_where` 一次处理所有标记。所以标记即边界，不依赖实体在不在某棵树里。这些系统由 `enable_state_scoped_entities::<GameLevel>` 注册；状态初始化时（`init_state`、`insert_state`、`add_computed_state`、`add_sub_state`）会自动调用它（`crates/bevy_state/src/app.rs`）。

重货（glTF 整关）走根实体：

```rust
commands.spawn((
    WorldAssetRoot(asset_server.load(
        GltfAssetLabel::Scene(0).from_asset("levels/level1.gltf")
    )),
    DespawnOnExit(GameLevel::Level1),
));
```

`WorldAssetRoot` 是“把这个 `WorldAsset` 实例化到当前实体下”的组件，`WorldAsset` 则是序列化后的实体树资产；实例化系统按这个句柄把资产里的实体树 spawn 成根实体的子节点。退出时根实体 despawn，`Children` 关系带 `linked_spawn`，整棵实例树一起走。也可以手动 `commands.entity(root).despawn()`，效果相同。

### 3. 消息层：上一关的“玩家死亡”不要漏进下一关

`Messages<M>` 是双缓冲消息队列，`MessageReader` 只读自己上次游标之后的消息；如果上一关末尾写了一条 `PlayerDied`，而下一关没人清理队列，这条消息会在新关的第一次读取里出现。用状态作用域消息扩展登记：

```rust
app.clear_messages_on_exit::<PlayerDied>(GameLevel::Level1);
app.clear_messages_on_enter::<LevelIntro>(GameLevel::Level2);
```

这两个方法来自 `StateScopedMessagesAppExt`（`crates/bevy_state/src/state_scoped_events.rs`），在 `OnExit`/`OnEnter` 调度里挂清理系统，把对应 `Messages<M>` 的队列清空。内部资源 `StateScopedMessages<S>` 是私有类型，按状态值存一组清理函数。

注意顺序：源码注释明确说消息清理与 `DespawnOnExit` 的实体清理、以及目标状态的 `OnExit` 之间顺序不确定，三者都在 `StateTransition` 的退出阶段里。需要“先读旧实体再清消息”这类顺序时，不要把它建立在调度顺序上。

### 4. 资产强柄层：实体清完，强柄自然掉零

关卡实体上的 `Mesh3d`、`MeshMaterial3d` 组件里是 `Handle`。`Handle::Strong` 包着 `Arc<StrongHandle>`；最后一份强柄 `Drop` 时只向 `AssetHandleProvider` 的 channel 发一个 `DropEvent`，真正的移除发生在下一次 `PreUpdate` 的 `Assets::<A>::track_assets`（系统集 `AssetTrackingSystems`）。所以“清实体就清资产”是自动的，游戏代码里没有 `release` 调用。

不会因为实体 despawn 而消失的，是你自己放在 Resource 里的强柄：例如把 `Handle<Gltf>`、预加载的 `Handle<WorldAsset>` 存进常驻 Resource。这些要显式 `remove_resource` 或 drop 掉，否则整个 glTF 家族会一直活着。0.20-dev 没有 `Assets::clear`；要主动清某一类资产，用 `Assets::<A>::remove(id)` 逐个移除，它会发 `AssetEvent::Removed`。

### 5. 系统私有状态层：Local 和关卡作用域资源

`Local<T>` 是系统自己的跨帧状态（`crates/bevy_ecs/src/system/system_param.rs`），初始化一次，之后每次系统运行拿到同一份；它不随实体 despawn 复位。宿主里的 `ResizeGate`、`FrameLogs`、`OcclusionGate`，上传链的 `UploadState`，都是这种状态。多数与关卡无关，跨关保留是对的；如果某个 `Local` 记的是“本关 Boss 已死”这类关卡事实，就要在 `OnExit(GameLevel::Level1)` 里显式复位。

Resource 没有内建的“状态作用域”版本。Bevy 0.20-dev 的 `state_scoped` 模块只提供实体标记组件（`DespawnOnExit` 等），不提供随状态自动清理的 Resource。关卡相关资源要么在 `OnExit` 里 `commands.remove_resource::<T>()`，要么把按关卡变化的数据收进一个容器，在切关时清空。`InstanceLedger` 的 DrawList 组装缓存（`AssemblyCache`）不用担心：它按 Entity 记账，实体 despawn 时 `update_ledger` 会清行并标记组装重估；账本里的累计计量（`stats`）是跨关保留的诊断数，不是关卡状态。

### 五层在同一次换关里的顺序

1. 玩法系统写 `NextState<GameLevel>`；
2. 下一帧 `PreUpdate` 之后，`StateTransition` 跑退出阶段：`OnExit(Level1)`、`DespawnOnExit(Level1)` 的实体清理、`clear_messages_on_exit` 的消息清空；
3. 同一调度里跑 `OnEnter(Level2)`，新关实体和 `WorldAssetRoot` 进场；
4. 被 despawn 的实体在命令应用时 Drop 组件里的 `Handle`，强柄计数减一；
5. 下一个 `PreUpdate`，`Assets::track_assets` 处理 `DropEvent`，从 `Assets` 容器移除资产，并在 `PostUpdate` 发出 `AssetEvent::Removed`；
6. 同一帧或下一帧的 `PostUpdate`，ash_renderer 的 `propagate_asset_changes` 把 `Removed` 记进 `AssetRetirements`；`Last` 的 `flush_uploads` 把驻留条目送进待回收账；
7. 再下一轮 `flush_uploads` 的排空点，等过所有在飞帧 fence（网格和贴图还要等各自的上传票据）后，槽位才真正可复用。

所以“换关释放内存”不是一个瞬间动作：CPU 侧最快也要跨一两个帧边界，GPU 侧还要多等一轮排空。要判断某一帧到底还没释放什么，就沿这条链看。

## 四、场景图：World 里是一片森林，不是一棵单根树

World 的实体表是平的，没有“场景根”这种结构；同一时刻可以有多棵互不相连的树：关卡根、玩家、UI、每个 `WorldAssetRoot` 实例、相机。所谓“进关挂同一个根”，是编排约定——为了让退场只发一个 despawn；引擎不要求所有实体同根，`DespawnOnExit` 更是按标记工作，不依赖树。

实体之间的关系由关系组件表示。`ChildOf(pub Entity)` 是“我挂在谁下面”，反向的 `Children(Vec<Entity>)` 由关系机制自动维护：

- 一个实体只有一个 `ChildOf`，即单父；替换父时旧父的 `Children` 会被同步移除；
- `Children` 声明了 `linked_spawn`（连带 despawn 标记）；父实体 despawn 时按 `RelationshipTarget` 的 `on_despawn` 钩子递归 despawn 子树（`crates/bevy_ecs/src/hierarchy.rs`、`crates/bevy_ecs/src/relationship/mod.rs`）；
- 需要“空间父 + 交互父”这类多重归属时，定义不同的关系组件，而不是给一个实体挂多个 `ChildOf`。

`WorldAssetRoot(pub Handle<WorldAsset>)` 是“一个资产实例”的根：根实体带 `Transform` 和 `Visibility`（`#[require(...)]`），spawner 用 `spawn_as_child` 把资产里的实体树实例化到它下面，完成后根实体挂 `WorldInstance`。关卡编排自己 spawn 的根是另一个根，两者平级；谁挂 `DespawnOnExit`，谁就是这一关的清理边界。

## 五、引用计数：掉零只清 CPU，GPU 驻留要按票据退场

### 强柄计数不在 Assets 里，掉零只发事件

`Assets<A>` 的字段是 `dense_storage`（按 `AssetIndex` 连续存值）、`hash_map`（按 `Uuid` 存值）、`handle_provider`、`queued_events` 和 `duplicate_handles`（`crates/bevy_asset/src/assets.rs`）。强柄计数不在这个容器里：

- `Handle::Strong(Arc<StrongHandle>)`，克隆一次 `Handle` 就是克隆一次 Arc；最后一个 Arc 的 `Drop` 触发 `StrongHandle::drop`；
- 通过 `Assets::get_strong_handle` 另外创建的强柄，会在 `duplicate_handles` 里给同一个 `AssetIndex` 记一份，`remove_dropped` 先减这个数，避免同一资产的多份独立强柄提前移除。

`StrongHandle::drop` 只做一件事：把 `DropEvent { index, asset_server_managed }` 发进 `AssetHandleProvider`（该资产类型的句柄分配器）的 channel（`crates/bevy_asset/src/handle.rs`）。实际移除在 `Assets::<A>::track_assets`：它在 `PreUpdate` 的 `AssetTrackingSystems` 里跑，排在 `handle_internal_asset_events` 之后；它从 channel 取事件，AssetServer 托管的句柄先走 `AssetInfos::process_handle_drop`（顺带删掉 path→id 和 AssetInfo），然后 `Assets::remove_dropped` 把值从 `dense_storage` 移除、回收索引，并压入 `AssetEvent::Unused`；如果资产确实存在，再压一条 `AssetEvent::Removed`（`crates/bevy_asset/src/assets.rs`）。

`AssetEvent` 消息由 `Assets::asset_events` 在 `PostUpdate` 的 `AssetEventSystems` 里统一发出。所以“掉零”到“容器里没有这份资产”之间隔着一个系统点，不是 `Drop` 当帧完成；它是确定的、无扫描的，但不是同步的。

### 实体 despawn 与 glTF 家族怎样自动掉零

柄住在组件里：despawn 实体时组件 Drop，`Handle` 的 Arc 计数减一。这就是“清实体就清资产”的全部机制，没有额外的 release 调用。

glTF 的家族关系有两层。`Gltf` 资产本身持有 `scenes: Vec<Handle<WorldAsset>>`、`meshes: Vec<Handle<GltfMesh>>`、`materials`、`nodes`、`skins`、`animations` 的强柄（`crates/bevy_gltf/src/assets.rs`）；而 `WorldAsset` 内部是一整个 `World`（`crates/bevy_world_serialization/src/world_asset.rs`），实例化出来的实体上又挂着 `Mesh3d`/`MeshMaterial3d` 的柄。丢掉最后一个 `Handle<Gltf>`，并且场景实例实体已经 despawn，强柄才会沿“Gltf → WorldAsset/GltfMesh/GltfMaterial → Mesh/Image”一层层掉零。只丢根柄但实例实体还在，视觉资产仍被实体上的柄持有，这是常见的一类“以为清了其实没清”。

### 路径缓存是身份去重，不是保活

同一个路径重复 `AssetServer::load`，`AssetInfos::get_or_create_path_handle_internal` 会走 `path_to_index` 的已存在条目，返回同一个 `AssetIndex` 和一个新强柄；资产还活着时不会重复读盘。这是身份去重。

但这份映射不保活：`AssetInfo` 里存的是 `weak_handle`（弱引用，不增加强柄计数），不是强柄；最后一份强柄掉零时，`process_handle_drop_internal` 会把 `AssetInfo` 和 `path_to_index` 条目一起删掉（`crates/bevy_asset/src/server/info.rs`）；下次进关再 `load` 同一路径，是一次新的加载。想让资产跨关常驻、避免重复读盘，要自己保留一份强柄（例如把 `Handle<Gltf>` 放进常驻 Resource，切关时不 remove），不能指望路径缓存替你保活。

### CPU 释放不等于 GPU 释放

引用计数能把 CPU 容器和 AssetServer 的路径账一起收掉，但它管不到 GPU 驻留。`Assets` 里移除的是 CPU 侧的资产值和它的元数据；ash_renderer 的 GPU 驻留是另一套账：

- `MeshPool` 按 `AssetId<Mesh>` 存顶点/索引区间，`ImageCache` 按 `AssetId<Image>` 存 `GpuImage` 和描述符槽；
- `InstanceTables` 的对象表按实例槽存对象记录（网格范围、材质槽号、包围盒）；
- `BindlessTextureTable` 的贴图槽、采样器槽是描述符集里的下标。

这些槽位在资产掉零后不会自动消失：在飞 draw 可能还在读旧区间或旧槽。正确的顺序是“先记账、等证据、再归还”，而不是在 `Drop` 里立刻销毁。

ash_renderer 的这条链是：

1. `propagate_asset_changes` 在 `PostUpdate`（排在 `update_ledger` 之后）读 `AssetEvent<Mesh>`、`AssetEvent<Image>` 的 `Removed`，把身份记进待回收账 `AssetRetirements`（`retire_mesh`/`retire_image`）；`Unused` 只计数，不动作。驻留管理器主动淘汰走 `eject_mesh`/`eject_image`，汇入同一本账。
2. `flush_uploads` 在 `Last`（驻留 tick `ResidencySet` 之后、`draw_frame` 之前）先 `drain_graveyards`，再 `reclaim_retired`：
   - `drain_graveyards` 先等所有在飞帧的 fence（`frames.wait_all_inflight`）；网格区间和旧图的排空还要再等上传票据（`uploader.wait_until(max_ticket)`），描述符槽和实例槽只需帧 fence；
   - `reclaim_retired` 对每个待回收 mesh 调 `MeshPool::retire`（区间连同上传票据进待回收队列，返回释放的字节数），对每个待回收 image 调 `ImageCache::retire`（`GpuImage` 进待销毁队列，`BindlessTextureTable::retire` 把贴图槽进待回收账，采样器按 `users` 归零才进待回收账）；
   - 账本里换下的实例槽由 `InstanceTables::retire_object_slot` 进对象表的待回收队列，下一次 `drain_graveyard` 等到所有在飞帧后归还。
3. 下一轮 `flush_uploads` 的 `drain_graveyards` 才真正销毁旧图、归还区间和槽位：`MeshPool` 的区间要同时等“帧 fence 不读了”和“上传票据完成了”，因为窗口最小化时可能只有上传提交、没有图形提交；`ImageCache` 等 `wait_draws` 加上最大的上传票据；`BindlessTextureTable::drain_retired_slots` 等 `wait_draws` 后销毁归零的采样器并归还槽；`InstanceTables::drain_graveyard` 等 `wait_draws` 后归还实例槽。

三条安全条件都落在这些入口的契约里：最后使用（fence）等过、上传票据等过、共享方退场（采样器按 `users` 归零，fallback 槽永不回收、也不交回）。槽位归还发生在这些条件满足之后，下一次分配才可能拿到它，所以旧 draw 不会指向新对象。

用一关的 boss 走一遍：Boss 实体在 `StateTransition` 的退出阶段被 `DespawnOnExit` 清掉 → 它组件里的 `Handle<Mesh>` 掉零 → 下一个 `PreUpdate` 的 `track_assets` 把 mesh 从 `Assets<Mesh>` 移除 → `PostUpdate` 发出 `AssetEvent::Removed` → `propagate_asset_changes` 记进 `AssetRetirements` → `Last` 的 `flush_uploads` 调 `MeshPool::retire` 入待回收队列 → 下一轮 `flush_uploads` 等过 fence 和上传票据后归还区间。如果这期间还有别的关卡实体引用同一 mesh，它不会掉零，`retire` 也不会发生——引用计数先挡在前面。

常驻表加槽位复用的好处在这里具体体现：换关只改数据槽位，场景管线和描述符集绑定不需要重建；反过来，如果每关都重建管线和 set，换关成本会落在最贵的对象上。

## 自测

1. 为什么换关不删主 World？World 里哪些东西不该跟着关卡清？
2. `DespawnOnExit` 为什么不需要所有实体同根？父实体 despawn 时子树靠什么一起走？
3. 强柄掉零之后，`Assets` 里什么时候才没有这份资产？`AssetEvent::Removed` 之后，GPU 槽位为什么还不能立刻复用？
4. 同一个 glTF 路径重复 `load` 为什么不会重复读盘？为什么这个缓存不能保证下一关也不读盘？

## 相关阅读

- 《[场景三层含义：glTF scene、WorldAsset与引擎World](../../开发步骤/3-静态取数链路/3.1-ECS侧取数/场景三层含义：glTF scene、WorldAsset与引擎World.md)》：World、WorldAsset、场景内容三层用词；本篇的 `WorldAssetRoot` 与“场景内容层”按这篇的口径使用。
- 《[图形管线机制：打包清单、双集ABI与bindless下的角色](../../开发步骤/3-静态取数链路/3.4-管线与绘制/图形管线机制：打包清单、双集ABI与bindless下的角色.md)》：渲染器作为系统的另一面，管线与描述符集的组织方式。
- 《[变更检测与资产事件](变更检测与资产事件.md)》：`AssetId` 身份/内容两层账、`AssetEvent` 家族语义；本篇只用到其中与换关清理相关的部分。

## 来源

- Bevy 源码（`0.20.0-dev` 本地快照，2026-11-19；行号会漂移，本文只引符号名）：
  - `crates/bevy_ecs/src/resource.rs`：`Resource: Component` 的 marker trait 定义；
  - `crates/bevy_window/src/window.rs`：`Window` 是实体组件，`PrimaryWindow` 标记主窗口；
  - `crates/bevy_ecs/src/system/system_param.rs`：`Local<T>`；
  - `crates/bevy_ecs/src/hierarchy.rs`、`crates/bevy_ecs/src/relationship/mod.rs`：`ChildOf`/`Children`、`linked_spawn` 与 despawn 钩子；
  - `crates/bevy_state/src/state_scoped.rs`：`DespawnOnExit`/`DespawnOnEnter`/`DespawnWhen` 及清理系统；
  - `crates/bevy_state/src/app.rs`：`enable_state_scoped_entities`、`StateTransition` 在 `MainScheduleOrder` 中的位置；
  - `crates/bevy_state/src/state_scoped_events.rs`：`StateScopedMessages`、`StateScopedMessagesAppExt`、`clear_messages_on_exit`/`clear_messages_on_enter`；
  - `crates/bevy_state/src/state/transitions.rs`：`StateTransitionSystems` 的退出/进入阶段顺序；
  - `crates/bevy_asset/src/assets.rs`：`Assets` 字段、`track_assets`、`remove_dropped`、`remove`；
  - `crates/bevy_asset/src/handle.rs`：`StrongHandle`、`DropEvent`、`AssetHandleProvider`；
  - `crates/bevy_asset/src/server/info.rs`：`path_to_index`、`process_handle_drop_internal`、`weak_handle`；
  - `crates/bevy_asset/src/lib.rs`：`AssetTrackingSystems`（`PreUpdate`）、`AssetEventSystems`（`PostUpdate`）；
  - `crates/bevy_render/src/lib.rs`：`RenderApp`、`ExtractSchedule` 与主 World 到渲染 World 的提取；
  - `crates/bevy_gltf/src/assets.rs`：`Gltf` 的 scenes/meshes/materials/nodes/skins/animations 强柄；
  - `crates/bevy_world_serialization/src/components.rs`、`world_asset.rs`、`world_asset_spawner.rs`：`WorldAssetRoot`、`WorldInstance`、`WorldAsset` 内嵌 `World`、`spawn_as_child`；
  - `crates/bevy_app/src/app.rs`、`crates/bevy_window/src/lib.rs`、`crates/bevy_winit/src/lib.rs`：`OnAppExitSystems` 与 `ExitSystems`/`despawn_windows` 的先后。
- 本仓代码（`ash_renderer`）：
  - `ash_renderer/src/driver/host/init.rs`：`init_vulkan` 的全有或全无、`init_chain!` 的拆除顺序、`teardown_vulkan`；
  - `ash_renderer/src/driver/host/mod.rs`：`draw_frame` 挂 `Last`、`teardown_vulkan` 挂 `OnAppExitSystems`；
  - `ash_renderer/src/driver/upload.rs`：`flush_uploads` 的注册与 `before(draw_frame)`、`drain_graveyards`、`reclaim_retired`；
  - `ash_renderer/src/scene/mechanism/assets/ledger.rs`：`InstanceLedger`、`InstanceRow` 的字段与“GPU 足迹不进行”的边界；
  - `ash_renderer/src/scene/mechanism/assets/asset_events.rs`：`AssetRetirements`、`retire_mesh`/`retire_image`、`propagate_asset_changes`；
  - `ash_renderer/src/render/resident/tables/bindless_texture_table.rs`：`retire`、`drain_retired_slots`、采样器 `users` 计数；
  - `ash_renderer/src/render/resident/tables/instance_table.rs`：`retire_object_slot`、`drain_graveyard`；
  - `ash_renderer/src/render/resident/assets/pool.rs`：`MeshPool::retire`、`drain_graveyard` 的双条件等待；
  - `ash_renderer/src/render/resident/assets/images.rs`：`ImageCache::retire`、`drain_graveyard`；
  - `ash_renderer/src/render/resident/slot_allocator.rs`：`release` 与 `retire` 的区别。
- 版本说明：2026-09-29 初稿由两轮问答整理；2026-11-19 按当前 `0.20.0-dev` 与 `ash_renderer` 源码重写，API 均按符号名核实。
