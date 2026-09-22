# 项目目标与 Bevy 使用指南

> 2026-09-14 建档。本文是 `F:\okzkx\bevy\.agent\docs` 知识库的第一篇/入口篇，写给未来的自己与未来的 agent 会话。
> 仓库现状：Bevy 上游浅克隆，tag **v0.19.1**（建档时）。**版本不钉死，跟随官方 release tag 升级**（2026-09-14 改策）：渲染插件按当前理解写，遇重大破坏性变更无法跟进时再钉死当时版本。选型论证在上游知识库（见文末"相关文档"），本文做三件事：**记录项目目标** + **讲清楚 Bevy 在本项目里怎么用** + **定文档分类归档规则**。
> 后续文档按中文标题命名（不编号）追加，并在文末登记；动笔前先按 §九 的生命周期分类对号入座。

## 一、项目目标（记录在案）

> **用 Bevy 作为后端（宿主引擎），我写 Rust Vulkan Bindless 前端渲染器，用来练习大世界 Rust Vulkan Bindless 渲染。**

拆成三个练习目标，全部在自研渲染器侧完成：

1. **Bindless**：descriptor indexing（`VK_EXT_descriptor_indexing`）起步，descriptor buffer 收尾，不建传统 bound 回退管线；
2. **GPU-driven**：剔除、indirect draw、LOD 决策尽量搬到 GPU；
3. **大世界数据获取与流送**：海量对象的组织、合并、按需加载。

明确**非目标**：不做通用引擎、不做商业化产品、不升级 winit 窗口层、不做 CPU 侧可见性剔除（全量交 GPU）、不做 UI（调试用 imgui）。

## 二、架构分工与数据流

```
┌────────────────────────────────────────────────────┐
│  Bevy       （宿主 / "后端"）                       │
│  ECS 世界 · winit 窗口 · bevy_asset + glTF          │
│  场景(BSN/DynamicScene) · 动画(CPU蒙皮) · 变换传播   │
│  灯光/相机组件 · bevy_city 式大世界组织层            │
└───────────────────────┬────────────────────────────┘
                        │  普通 system 直读（无 RenderApp、无跨 World 提取）
┌───────────────────────▼────────────────────────────┐
│  自研 ash 渲染插件（"前端"，本项目练习本体）          │
│  场景采集(Query) → 增量上传(变更检测)                 │
│  → bindless 描述符 → GPU 剔除/indirect → VkQueue     │
└────────────────────────────────────────────────────┘
```

数据流五步：

1. bevy_gltf 异步加载场景 → spawn 实体（`Mesh3d` / `MeshMaterial3d<StandardMaterial>` / `Handle<Image>` / `GlobalTransform` / `ChildOf`…）；
2. 自研渲染系统（普通 system）挂 `PostUpdate`，直接 `Query` 采集本帧可见数据；
3. 用变更检测（`Added` / `Changed` / `RemovedComponents`）算出增量，驱动 buffer/贴图上传；
4. 写 bindless 描述符，提交 `vkQueueSubmit`，画到 winit 窗口的 VkSurface；
5. 与官方 bevy_render 最大的架构差异：官方为"主世界/渲染世界并行"做跨 World 提取（Extract），我们**单 World 同步直读**——简单直接，这正是选自研的收益之一。

### 渲染前准备：数据侧架构定案（2026-09-14 讨论记录）

**单 World 直读 + 变更检测 + bindless 常驻池**——这是渲染前准备工作（数据侧）的完整架构，也是练习目标在数据侧的落点：

1. **bindless 常驻池**：真正的"镜像"建在 GPU 上——mesh/纹理/实例数据进 bindless 大池，一次上传长期驻留；CPU 不养 UE 式场景镜像。常驻是默认，流送换出是它之上的管理层（大世界练习项之一）；
2. **变更检测 = 脏标记**：Bevy `Changed<T>`/`Added<T>`/`RemovedComponents<T>` 天然就是脏标记来源，只上传这帧变过的数据——借用官方 Extract 的思想，但同 World、零拷贝、只服务上传；
3. **GPU 每帧自建可见集**：compute 剔除 → 生成 indirect 参数 → bindless 索引绘制，CPU 永不做每对象装配。

背景判断：现代引擎（UE5 Nanite、Unity 6 GPU Resident Drawer）的演进方向是 CPU 每对象成本趋近零，只剩"资产驻留管理 + 脏数据推送"，"双 World 镜像 vs 帧末直读"之争被 GPU-resident 消解。本选型 = Unity 帧末装配的直读习惯 + 现代 GPU-resident 数据层。里程碑对应：M2/M3 实现第 1、2 层，M4/M5 实现第 3 层。

## 三、用法边界：用什么、怎么用、不碰什么

| 模块 | 用法 |
|---|---|
| `bevy_app` + `DefaultPlugins` | 组装入口：`DefaultPlugins.build().disable::<RenderPlugin>()`，只禁渲染后端（`disable` API 在 `crates/bevy_app/src/plugin_group.rs:501`） |
| `bevy_ecs` | 全用。渲染采集/上传就是普通 system |
| `bevy_winit` | **保留**（窗口创建 + 事件循环）。已核实其依赖只有 `bevy_window` + winit 0.30（rwh_06），**不依赖 bevy_render**，禁渲染不影响它；源码不动、winit 版本不升级 |
| 窗口 → VkSurface | `WinitWindows`（NonSend 资源，`crates/bevy_winit/src/winit_windows.rs`）拿 winit 窗口 → raw-window-handle（rwh_06）→ ash 建 `VkSurfaceKHR` |
| `bevy_asset` + `bevy_gltf` | 资产入口，glTF only（bevy_gltf 依赖 bevy_image/bevy_mesh，无渲染耦合）；FBX 不兼容 |
| `bevy_scene` / BSN | 场景组织：`bevy_scene::bsn` 宏 + `.bsn` 资产 + `DynamicScene` 序列化 |
| `bevy_transform` | `GlobalTransform` 传播由 `TransformPlugin` 挂在 **PostUpdate**（`crates/bevy_transform/src/plugins.rs`）；大世界用 `StaticTransformOptimizations::Enabled` 跳过静态子树重传播 |
| `bevy_animation` | CPU 蒙皮在主世界跑，渲染侧只取结果 |
| `bevy_pbr` / `bevy_render` | **不链接不运行，只读源码抄架构**（GPU 驱动内核见第七节）。编译期 wgpu 仍在依赖树里，运行时不初始化 |
| bevy 版本策略 | **不钉死，跟随官方 release tag 升级**（2026-09-14 改策，原为钉死 0.19.1）；渲染插件按当前理解写，遇重大破坏性变更无法跟进时再钉死当时版本。升级后需复核本文标注"0.19.1 已核实"的 API 细节 |

若 M1 实测发现还有其它渲染族插件因缺 RenderApp 报错，同样 `.disable::<它们>()` 掉即可（`disable` 机制就是为这种裁剪设计的）。

**自有参照工程：frenderer**（`F:\okzkx\rust-frenderer`，ash 0.37 + winit 0.28.6）——传统 Vulkan bind 管线渲染器，在产工具：**不重写不迁移**，作本项目技术参考与 bound 管线 A/B 基准；其 FBX 资产管线不迁移，本项目资产走 glTF only。

## 四、Bevy 核心概念速成（按本项目使用顺序）

1. **App 与 Plugin**：一切从 `App::new().add_plugins(...)` 开始；自研渲染器就是一个普通 `Plugin`——build 时建 VkInstance/Device、插资源、注册系统。参考 `examples/app/empty.rs`。
2. **ECS 三件套**：Entity（id）/ Component（数据）/ System（函数，参数即查询）。系统参数：`Query`、`Res`/`ResMut`、`Commands`、`Local`、`NonSend`。完整教程见 `examples/ecs/ecs_guide.rs`。
3. **Schedule 与排序**：`Startup` → 每帧 `PreUpdate`/`Update`/`PostUpdate`/`Last`。渲染采集挂 `PostUpdate`；与变换传播的顺序用 `.after(bevy_transform::TransformSystems::TransformPropagate)` 显式约束（注意 0.19.1 枚举名是复数 `TransformSystems`，`crates/bevy_transform/src/plugins.rs:13`）。链式约束 `.before`/`.after` 是唯一的确定性排序手段。
4. **变更检测（增量上传的核心）**：`Added<T>`（首见）、`Changed<T>`（写访问触发）、`RemovedComponents<T>`（删除，用 Drains 迭代）。关键规则：只有 `DerefMut` 式可变访问才打 Changed 标记，读数据的系统别拿可变引用。参考 `examples/ecs/change_detection.rs`、`examples/ecs/removal_detection.rs`。
5. **资产系统**：`Handle<T>` 是弱引用，真数据在 `Assets<T>` 资源里。glTF 加载是异步的——spawn 后前几帧 `Assets::get()` 可能是 `None`，采集系统必须容忍；文件变更默认自动热重载。参考 `examples/asset/asset_loading.rs`、`examples/asset/hot_asset_reloading.rs`。
6. **变换层级**：`ChildOf`/`Children` 关系 + `Transform`（本地）→ `GlobalTransform`（传播结果）。传播在 PostUpdate，所以渲染系统排其后拿到的才是本帧最终矩阵。参考 `examples/ecs/hierarchy.rs`、`examples/3d/parenting.rs`。
7. **消息与 Observer**：0.17 起事件更名 Message（`MessageReader`/`MessageWriter`）；Observer 是"实体级回调"，bevy_city 用消息驱动加载流 + observer 补组件。参考 `examples/ecs/message.rs`、`examples/ecs/observers.rs`。
8. **场景**：`bevy_scene::bsn!` 宏（声明式场景，`examples/scene/bsn.rs`）；`DynamicScene` 序列化（`examples/scene/world_serialization.rs`）。
9. **调度器手动驱动（备选知识）**：官方两种 headless（`examples/app/headless.rs` 纯无窗口；`examples/app/externally_driven_headless_renderer.rs` 手动泵 `SubApps`）都仍走 bevy_render 的 RenderApp，**与本项目模式不同**，读它们是为了对照"官方跨 World 提取长什么样"（`examples/app/headless_renderer.rs` 是 Extract 模式的完整示范），不是照抄。

## 五、渲染侧取数速查表（0.19.1 已核实，升级后复核）

| 需求数据 | 从哪取 |
|---|---|
| 相机 | `Camera` + `Projection` + `GlobalTransform`（清除色/视口/HDR 都在组件上） |
| 网格 | `Mesh3d(Handle<Mesh>)` → `Assets<Mesh>`（POSITION/NORMAL/TANGENT/UV_0/JOINTS_0/WEIGHTS_0 + `Indices`） |
| 材质 | `MeshMaterial3d<StandardMaterial>` → `Assets<StandardMaterial>`（base_color、metallic/roughness、`alpha_mode` 决定管线） |
| 贴图 | `Handle<Image>` → `Assets<Image>`（像素数据 + `TextureFormat` + sampler 设置，自建 VkImage） |
| 层级 | `GlobalTransform`（已传播）+ `ChildOf`/`Children` |
| 灯光 | `PointLight` / `SpotLight` / `DirectionalLight` / `AmbientLight` / `EnvironmentMapLight` |
| 蒙皮 | `SkinnedMesh`（动画在 CPU 算完，渲染侧取关节矩阵结果） |
| 场景 | bevy_gltf spawn + `bsn!` / `DynamicScene` 序列化 |

## 六、必跑示例清单（从简到繁）

```bash
# 在 F:\okzkx\bevy 下
cargo run --example hello_world          # 最小 App
cargo run --example 3d_scene             # 最小 3D：mesh+material+light+camera
cargo run --example parenting            # 层级
cargo run --example load_gltf            # glTF 加载
cargo run --example query_gltf_primitives# 遍历 glTF 网格/材质实体（取数前哨）
cargo run --example change_detection     # 变更检测
cargo run --example observers            # Observer
cargo run --example bsn                  # BSN 场景
cargo run -p bevy_city --release -- --no_cpu_culling --size 50   # 大世界参照
```

分层精读（不必全跑，按里程碑挑）：

- **宿主组装**：`examples/app/empty_defaults.rs`、`examples/app/custom_loop.rs`、`examples/app/headless_renderer.rs`（对照 Extract 模式）
- **大世界组织层**：`examples/large_scenes/bevy_city`（消息驱动加载流、`merge_car_meshes` 网格合并、`StaticTransformOptimizations::Enabled`、observer 补组件）+ `examples/large_scenes/bistro`、`caldera_hotel`（更大场景）
- **GPU-driven 内核（只读架构）**：`crates/bevy_pbr/src/render/mesh_preprocess.wgsl` → `build_indirect_params.wgsl` → `gpu_preprocess.rs`；`crates/bevy_render/src/batching/gpu_preprocessing.rs`（约 2756 行，主实现）
- **LOD 思路**：`examples/3d/visibility_range.rs`；遮挡：`examples/3d/occlusion_culling.rs`；meshlet：`examples/3d/meshlet.rs`

## 七、里程碑（建议顺序）

| 里程碑 | 内容 | 验证点 |
|---|---|---|
| M1 宿主壳 | `DefaultPlugins.build().disable::<RenderPlugin>()` + winit 窗口 + ash Instance/Device/Swapchain + 清屏循环 | 窗口生命周期、resize、与 ECS 调度共存；确认无渲染族插件 panic |
| M2 静态取数 | glTF 场景 → Query 采集 → 上传 → descriptor indexing 画静态场景 | 与 bevy 默认渲染器同屏对比正确性（临时开 wgpu 跑对照） |
| M3 增量链路 | `Changed<GlobalTransform>` 等驱动增量上传 + 资产热重载联动 | 改 glTF 文件/移动物体，帧内生效 |
| M4 大世界 | 抄 bevy_city 组织层 + 抄内核 GPU 剔除架构（wgsl→GLSL/内嵌 WGSL 均可） | 10 万级实体流畅 |
| M5 GPU-driven 全链 | indirect draw、遮挡剔除、LOD；descriptor buffer 收尾 | 参照外部 Bindless/GPU 驱动参考清单 |

## 八、已知坑与注意

- **禁渲染模式未经本项目实测**（bevy_city 跑的是完整 wgpu 栈）——M1 第一件事就是验证；`disable::<RenderPlugin>()` 是官方支持的 API，预期可行。
- glTF spawn 后资产异步到达，首帧 `Assets::get()` 为 `None` 是正常现象，采集系统要容忍空帧。
- `Changed` 由可变访问触发：只读系统误拿 `ResMut`/`&mut` 会污染变更检测，制造假上传。
- 渲染系统必须排在变换传播（PostUpdate 内）之后，否则拿到上一帧矩阵。
- 浅克隆仓库：`git log`/`bisect` 深挖历史前需 `git fetch --unshallow`，或直接查 GitHub。
- Bevy 不做路线图、无 1.0 时间表——以本仓库 checkout 的源码为唯一权威，别拿文档/教程直接对号入座。升级操作（浅克隆）：`git fetch --depth 1 origin tag vX.Y.Z && git checkout vX.Y.Z`；每次升级后复核本文与《学习目标实现步骤》中标注"0.19.1 已核实"的 API 细节（易变项如 `TransformSystems` 复数命名、`shadow_maps_enabled` 等改名）。

## 九、文档分类归档规则（2026-09-22 定案）

> 全目录文档盘点后的定案：**新文档动笔前先按生命周期对号入座**，写完在文末登记；混血文档（设计文档里嵌教程、结果文档里嵌图谱）按主用途归一类。落位（2026-09-22 三次修订）：**根目录 = 总览层，平铺不设分类文件夹**——入口篇 + 路线图少而重要，平铺一眼可见；跨步骤专题笔记（宪法/方法论/工具层/侦察）集中 `笔记/`，不占根目录；**分类的主战场在 步骤文件夹内部**——步骤文件夹根放 README（材料目录）与该步主产出/主计划，其余专题文档（侦察/设计/知识点/手册）全进 `材料/`；施工段文件夹（N.M-*）照旧平级。
>
> ```
> .agent/docs/
> ├── 项目目标与Bevy使用指南.md    ← 入口（总览层，平铺）
> ├── 学习目标实现步骤.md          ← 路线图
> ├── 笔记/                        ← 跨步骤专题笔记（宪法/方法论/工具层/侦察）
> │   ├── 错误处理体系：两Tier思想与优雅退出.md   ← 宪法篇
> │   ├── 写文档纪律：帧流程三轮返工的教训.md     ← 方法论篇
> │   ├── 开发工具与语法笔记.md        ← 工具层知识
> │   ├── 错误处理语法糖：frenderer syntax与ash_renderer移植.md  ← 工具篇
> │   └── bevy_remote：BRP远程协议与自定义方法.md  ← 侦察篇（Web Console 候选底座）
> └── N-*/
>     ├── README.md                ← 材料目录（索引）
>     ├── <主产出/主计划>           ← 该步最重要文档，留根（1 笔记 / 2 搭建记录 / 3 施工计划）
>     ├── 材料/                     ← 其余专题文档（侦察/设计/知识点/手册），按需翻查
>     └── N.M-*/                    ← 施工段工作区（步骤 3 起，纯数字前缀）
> ```
>
> 本文管"归哪类、放哪里"；"怎么写"见《[写文档纪律：帧流程三轮返工的教训](笔记/写文档纪律：帧流程三轮返工的教训.md)》。

### 事前——开工前立四件

| 类型 | 职责 | 实例 |
|---|---|---|
| 路线图 | 六步总纲，管 步骤级状态（⬜🚧✅） | 《学习目标实现步骤》 |
| 侦察/调研 | 带着施工问题读源码，答案直接喂给设计 | 步骤 2《窗口链路侦察》、步骤 1《glTF加载链路》 |
| 设计文档 | 定终态与为什么，含待决问题 | 步骤 3《施工计划》、步骤 2《Context与Swapchain分家》 |
| 施工任务面板 | 开工工作单：目的/任务清单/验证标准；**事前立、事中活、收官封存** | 步骤 3 施工 3.1~3.5 README |

### 事中——施工中动两处

1. **任务面板推进**：勾 checkbox、改状态、补材料清单，不为此新开文档；
2. **配套知识/参考手册随施工写**：施工中长出来的理解沉淀成篇（如《VulkanContext字段释义》之于施工 2.3）。

### 事后——收官收三样

| 类型 | 职责 | 实例 |
|---|---|---|
| 施工结果 | 做了什么+证据，可实测复跑 | 《2-宿主壳搭建记录》§4 |
| 总结 | **不设独立文档**——步骤收官时重写进施工结果，一文档两职 | 同上（2026-09-21 收官重写） |
| 知识点沉淀 | 机制理解，可独立于施工传播 | 《帧流程》《PipelineStage》及 步骤 1 全部 |

### 贯穿——持续维护三份

| 类型 | 职责 | 实例 |
|---|---|---|
| 索引/导航 | 入口篇 + 各步骤 README（材料目录），新文档在此登记 | 本文、各步骤 README |
| 宪法/规范 | 跨施工通用定案原则，所有工程默认遵守 | 《错误处理体系：两Tier思想与优雅退出》 |
| 方法论/纪律 | 管"文档怎么写"这件事本身 | 《写文档纪律：帧流程三轮返工的教训》 |

### README 材料清单后缀对齐

材料条目"——XX篇"后缀与上表类型挂钩，固定六件：侦察=**开篇侦察**、设计=**施工计划/设计**、配套=**施工N配套**、施工结果=**主产出**、宪法=**宪法篇**、方法论=**方法论篇**；知识点类自由命名（原理篇/关系总览/同步语义篇/工具篇……）；路线图与任务面板不进材料清单（各有登记位）。

### 步骤编号体系（2026-09-22 定案）

施工序列用**层级累积编号**，父号打头、全局唯一、永不重数：L1 步 `N`（文件夹 `N-*`，行文"步骤 N"）→ L2 段 `N.M`（文件夹 `N.M-*`，行文"施工 N.M"）→ L3 任务 `N.M.K`（任务面板 checkbox）→ L4+ 续数字不设专名。里程碑 M1~M5 是验证刻度不参与编号；文档自身章节号（§N）与步骤编号无关。定案全文与历史映射（步骤 2 施工①~④ = 2.1~2.4）见《[学习目标实现步骤](学习目标实现步骤.md)》"编号约定"节。

## 相关文档

本目录（新文档在此登记）：

- `项目目标与Bevy使用指南.md`（本文）
- `学习目标实现步骤.md`（执行路线图：六步总览 + 每步目的/做什么/产出/完成标准；含"编号约定"节）
- `1-熟悉Bevy结构/Bevy结构笔记.md`（ECS 核心概念笔记：App/SubApp/Schedule/World + 一帧时间线 + runner 真身表 + update() 内部三层，随学习进度增补）
- `1-熟悉Bevy结构/材料/SubApp机制与取舍.md`（SubApp 定位与判断标准、并行性真相、开销与风险账、对本项目的演进结论；§7 补遗：3d_scene 三 SubApp 创建者与渲染线程交接）
- `1-熟悉Bevy结构/材料/Plugin与PluginGroup机制.md`（Plugin 添加=build 与生命周期四段、Plugin 添加 Plugin 的两种形式、`plugin_group!` 宏展开（`:::`/`#[custom]`）与 set/disable 链、disable::<RenderPlugin> 的完整语义）
- `1-熟悉Bevy结构/材料/DefaultPlugins分类.md`（Q2：渲染族禁用名单 8 个、0.19.1 数据/渲染 crate 解耦、PbrPlugin 优雅降级核实、M1 验证清单）
- `1-熟悉Bevy结构/材料/BSN场景语法与Unity场景对比.md`（BSN 是什么/怎么用/功能盘点 + 与 Unity scene YAML 逐维度对比；Template/HandleTemplate/补丁机制；Q4 消费端半篇）
- `1-熟悉Bevy结构/材料/Bevy编辑器路线与代码热重载.md`（Bevy editor-last 立场三座山、BRP/.bsn 基建现状；Rust 热重载三堵墙与原生热重载光谱；数据热/逻辑冷分工）
- `1-熟悉Bevy结构/材料/数据层全景：World、Entity、Resource与Asset.md`（2026-09-16 合并原《World与Resource》《Entity与Asset》去重：World 字段解剖、Entity 位布局/世代防悬空、Resource=隐形实体+唯一性原理、Assets 双存储/Handle 强弱、tick 变更检测、数据放哪三层判定、同构表与 bindless 池落点）
- `1-熟悉Bevy结构/材料/System与调度DSL.md`（add_systems 四跳链、ScheduleLabel/Interned、System 运行时对象、IntoScheduleConfigs 配置树 DSL、复杂传参三层机制、建图→编译→执行、Schedule 心智模型与 Unity DOTS 对照、并行规则、对 barrier 推导的落点）
- `1-熟悉Bevy结构/材料/System参数与数据访问.md`（0.19.1 参数全量目录：Query D/F 模板与 Single/Populated 验证跳过、资源类/命令类/消息事件类/独占类/工具类/插件型参数、Commands 同步点三时机与手动控制、Local 详解、多 Query 共存规则、高频问答速查）
- `1-熟悉Bevy结构/材料/实体写路径：Bundle、Commands与Query改值.md`（spawn 的 Bundle 语义=静态组件清单一步落位 archetype、Commands API 全貌与错误处理、"不走 Commands"的独占系统、Query `&mut` 值路径与自动变更检测、结构路径 vs 值路径对比表与选型口诀）
- `1-熟悉Bevy结构/材料/事件系统全景：Message队列与Observer回调.md`（Messages 双缓冲+游标机制、Event/Observer 同步触发链路与防重入、一帧时序对比、ProjectStorm 实体事件对照与借鉴点）
- `1-熟悉Bevy结构/材料/glTF加载链路：从磁盘到Mesh3d.md`（Q4 收尾：AssetServer→GltfLoader→WorldAsset→WorldAssetRoot 五站、RenderAssetUsages 声明式数据用途、0.19 材质解耦与 GltfExtensionHandler 接入缝、frenderer 同链路对照与资源管理层薄弱点）
- `1-熟悉Bevy结构/材料/关系与层级：ChildOf、Children与变换传播.md`（待深入⑥收尾：与 Unity ECS 同构对照、Relationship 框架 hook 自动维护、变换传播三系统、静态子树跳过）
- `笔记/开发工具与语法笔记.md`（**非引擎结构**的工具层知识：Rust 宏语法、cargo、调试工具等，随读源码沉淀）
- `笔记/错误处理语法糖：frenderer syntax与ash_renderer移植.md`（frenderer 自写错误处理宏语法糖全图谱：anyhow 单类型底座 + LogDebug 扩展 + 9 控制流宏（8 件原件 + ash_renderer 新增 unwrap_or_panic!；前缀定输入/后缀定出口）、"错误就地消化"哲学、ash_renderer 移植版四适配与未搬三件——**工具篇**）
- `笔记/错误处理体系：两Tier思想与优雅退出.md`（**宪法篇**：用户两 Tier 错误处理思想（非必要不 panic——①不影响运行 warn 丢弃继续 ②影响运行冒泡 main 优雅退出）、VulkanError 类型层、try_init `?` 串链 + AppExit 优雅退出全链、`?` 可用=失败处理集中、两 Tier 实测证据、后续纪律）
- `笔记/写文档纪律：帧流程三轮返工的教训.md`（**方法论篇**（2026-09-22）：帧流程篇同日三轮返工的沉淀——机制先于比喻、判定线加粗立 §0、对着读者的下一个问题写、精简=删重复不删信息；配图三层验证与箭头锚点钉死规则）
- `笔记/bevy_remote：BRP远程协议与自定义方法.md`（**侦察篇**（2026-09-22）：bevy_remote=可选插件，把 App 经 JSON-RPC/HTTP 暴露成"ECS 服务端"（127.0.0.1:15702）——架构两层（协议/传输解耦、handler=system 帧节拍执行）、内置方法全景、反射边界（存在性全量/取值需 ReflectComponent）、自定义方法（构建期 with_method_main + 运行期 RemoteMethods::insert，handler 可独占 &mut World）、Web Console 落点（CORS Headers 已留口未验证、无鉴权仅限本地、渲染器内部状态应注册成自定义方法暴露）；0.20 方法名已整体改名 world.* 系列，旧资料照抄会 METHOD_NOT_FOUND）
- `3-静态取数链路/`（M2 施工目录，2026-09-22 开工：《施工计划：Bindless起步五段拆解》= bind 模型痛点→descriptor indexing 四件套入门 + 终态设计决策表 + 五段拆解（一次一段节奏）+ 已核实事实清单；索引见该目录 README）

上游知识库（`C:\Users\zengkaixiang\.agents\docs\游戏制作\游戏引擎\渲染\渲染管线\`，绝对路径引用）：

- 《Bevy 大世界渲染选型与自研渲染器决策》——选型论证、决策清单、bevy_city 实操结论
- 《自研渲染器宿主引擎选型：Unity、Godot 与 Bevy》——三宿主对比与"练习价值/商业价值"拆账
- 《技术美术\Bindless 与 GPU 驱动渲染参考》——内核文件定位、官方 PR、生态项目、阅读顺序
