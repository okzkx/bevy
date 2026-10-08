# 项目目标与 Bevy 使用指南

> 2026-09-14 建档。本文是 `F:\okzkx\bevy\.agent\docs` 知识库的第一篇/入口篇，写给未来的自己与未来的 agent 会话。
> 仓库现状：Bevy 上游浅克隆，tag **v0.19.1**（建档时）。**版本不钉死，跟随官方 release tag 升级**（2026-09-14 改策）：渲染插件按当前理解写，遇重大破坏性变更无法跟进时再钉死当时版本。选型论证在上游知识库（见文末"相关文档"），本文做三件事：**记录项目目标** + **讲清楚 Bevy 在本项目里怎么用** + **定文档分类归档规则**。
> **2026-09-23 当前基线：Bevy 0.20.0-dev / ash 0.38 / Vulkan 1.3。**上行 v0.19.1 是建档历史，不作当前 API 依据。执行顺序与状态见[路线图](学习目标实现步骤.md)；既有同步问题见[已实现缺陷与修复验收](3-静态取数链路/材料/已实现缺陷与修复验收.md)。本轮只修文档，未修代码。
> 文档按 §九 落位：施工记录带任务号，步骤主产出带步骤号，讲解篇不强加任务号；新文登记入口与所在目录索引。

## 一、项目目标（记录在案）

> **用 Bevy 作为后端（宿主引擎），我写 Rust Vulkan Bindless 前端渲染器，用来练习大世界 Rust Vulkan Bindless 渲染。**

拆成三个练习目标，全部在自研渲染器侧完成：

1. **Bindless**：descriptor indexing 起步，不建传统 bound 回退管线；descriptor buffer 保留为独立学习实验，按能力与实测决定是否替换，不作 GPU-driven 的必经终点；
2. **GPU-driven**：剔除、indirect draw、LOD 决策尽量搬到 GPU；
3. **大世界数据获取与流送**：海量对象的组织、合并、按需加载。

明确**非目标**：不做通用引擎、不做商业化产品、不升级 winit 窗口层、不做 CPU 侧可见性剔除（全量交 GPU）、不做 UI（调试用 imgui）。

## 二、架构分工与数据流

| 层 | 负责什么 | 不负责什么 |
|---|---|---|
| Bevy 宿主 | ECS、winit、资产加载、WorldAsset 实例化、变换传播、相机/灯光数据；若未来做动画则更新关节姿态 | 禁用的官方渲染插件不替我们上传、蒙皮顶点或绘制 |
| 自研 ash 渲染器 | 场景数据采集、GPU 资源与生命周期、bindless、剔除/indirect、提交与呈现 | 不照搬整套 RenderApp，也不重写通用宿主引擎 |

目标数据流：

1. bevy_gltf 加载 glTF 并实例化 WorldAsset；实体带 `Mesh3d`、`MeshMaterial3d<StandardMaterial>`、`GlobalTransform` 等，贴图柄经材质查找，不假定每个实体都有 `Handle<Image>` 组件。
2. 普通 system 在 PostUpdate、变换传播之后读取本帧场景内容；这是候选渲染数据，不等于已经完成可见性剔除。
3. M2 用全量拓扑快照加驻留缓存；M3 再结合对象变更、`AssetEvent`、异步就绪和删除记录构建增量。
4. Last 准备并提交本帧上传/绘制，GPU 通过同步依赖等待资源；同帧 CPU 采集不代表 GPU 同步完成。
5. 我们选择**单 World 帧序直读**，官方 Extract/RenderApp 仅作并行架构参照；M4 再让 GPU 自建可见集与 indirect，不回读可见集决定逐对象 draw。

### 渲染前准备：数据侧架构定案（2026-09-14 讨论记录）

**单 World 直读 + 增量上传 + bindless 常驻池**是本项目的数据侧方向，不代表所有中间数据都零拷贝，也不免除 CPU 资源管理。

1. **GPU 常驻**：mesh/纹理/实例数据按需上传并复用；CPU 仍保存资产到 GPU 资源、实体到实例槽、上传票据和最后使用记录，不养完整的第二个引擎 World。
2. **可靠增量**：对象变更、资产内容事件、异步到货和删除取消共同驱动更新。`Changed<T>` 仍扫描匹配实体，不能把“只上传变化项”写成“只遍历变化项”；当前快照和句柄 clone 也不是零拷贝。
3. **GPU 自建可见集**：步骤 5 以常驻实例表、包围体、GPU frustum 和 indirect 形成闭环，替换 M2 的逐 draw 参数装配；步骤 6 再加遮挡、LOD 和预算流送。

这是一种适合本学习项目的取舍，不宣称所有现代引擎都无需 CPU 镜像。GPU 常驻不自动解决主世界并行读写；本项目用同帧帧序消除竞争，以测量区分扫描、更新和提交成本。

## 三、用法边界：用什么、怎么用、不碰什么

| 模块 | 用法 |
|---|---|
| `bevy_app` + `DefaultPlugins` | 组装宿主，当前禁渲染族 8 项及连带 PbrPlugin，不是只禁一个 RenderPlugin 就完成全部裁剪 |
| `bevy_ecs` | 采集和上传准备是普通 system；PostUpdate 采、Last 提交 |
| `bevy_winit` | 保留窗口与事件循环，不主动升级 winit；本 fork 已为最小化消息做过定点修正，不再声称源码从未改动 |
| 窗口 → VkSurface | `PrimaryWindow` 的 `RawHandleWrapper` → raw-window-handle 0.6 → 手写 Win32 Surface，不依赖旧 NonSend WinitWindows 路径 |
| `bevy_asset` + `bevy_gltf` | glTF 资产入口，资产形态叫 WorldAsset；加载/图像解码需保留禁渲染后的必要注册 |
| 场景组织与序列化 | 当前使用 `WorldAssetRoot`；BSN 与世界序列化作按需知识，具体 API 以当前 checkout 为准，不继续按旧 DynamicScene 名称施工 |
| `bevy_transform` | PostUpdate 传播 `GlobalTransform`；静态子树优化是大场景候选，先测成本 |
| `bevy_animation` | 更新动画属性/关节姿态，不等于 CPU 已完成顶点蒙皮；未来自研渲染器仍需处理关节矩阵和蒙皮，主线暂不做 |
| `bevy_pbr` / `bevy_render` | 禁运行官方渲染插件；当前仍依赖 bevy_pbr 数据与材质转换，wgpu 在依赖树中但自研进程不初始化；GPU 架构仅参照 |
| Bevy 版本 | 保留跟随 release 的策略；升级单列任务并复核 fork/API，不在施工中隐式追新 |

禁渲染模式已实测成立；后续出现原渲染插件承担的资产职责，先查注册/finish 生命周期，再决定接管，不机械地继续禁插件。3.2 起是自己的渲染管线设计，不是复刻官方的“补位”。

**自有参照工程：frenderer**（`F:\okzkx\rust-frenderer`，ash 0.37 + winit 0.28.6）——传统 Vulkan bind 管线渲染器，在产工具：**不重写不迁移**，作本项目技术参考与 bound 管线 A/B 基准；其 FBX 资产管线不迁移，本项目资产走 glTF only。

## 四、Bevy 核心概念速成（按本项目使用顺序）

1. **App 与 Plugin**：`build` 注册插件、系统和资源；当前 Vulkan 在 Startup、窗口句柄已存在后初始化，不在 Plugin::build 时建 Surface/Device。
2. **ECS 三件套**：Entity、Component、System；参数声明读取和写入能力。参考 `examples/ecs/ecs_guide.rs`。
3. **Schedule 与排序**：采集用 `.after(TransformSystems::Propagate)`，draw 在 Last、退出拆除之前；用 before/after、chain 和系统集明确顺序，不依赖注册先后。
4. **变更检测**：`Added<T>` / `Changed<T>` 用 tick 过滤，仍扫描匹配实体；获取包装的可变访问不等于立即变化，解引用为可变访问、插入或显式标记等可触发。`RemovedComponents<T>::read()` 给删除记录，不提供旧组件值。
5. **资产系统**：真数据在 `Assets<T>`。`Handle::Strong` 保活资产，`Handle::Uuid` 不保活；异步未到货的 `Assets::get()==None` 是正常状态。文件监听通常需 `file_watcher`（启用 watch），`AssetPlugin.watch_for_changes_override=None` 跟随 feature，`Some(false)` 可关闭；不能无条件称默认热重载。
6. **变换层级**：ChildOf/Children 与 Transform 传播为 GlobalTransform；渲染在传播后读取最终矩阵。
7. **消息与 Observer**：Message 是缓冲消息路径；Event/Observer 是触发路径，两者并存，不能把所有 Event 说成改名 Message。
8. **WorldAsset**：glTF `scenes[]` / `#Scene0` 是资产内部组织；WorldAsset 是可实例化资产，运行时引擎 World 是全量数据库。整 WorldAsset 重载可能先 despawn 再 spawn，不默认保留 Entity 身份和额外状态。
9. **调度器手动驱动（备选知识）**：官方两种 headless（`examples/app/headless.rs` 纯无窗口；`examples/app/externally_driven_headless_renderer.rs` 手动泵 `SubApps`）都仍走 bevy_render 的 RenderApp，**与本项目模式不同**，读它们是为了对照"官方跨 World 提取长什么样"（`examples/app/headless_renderer.rs` 是 Extract 模式的完整示范），不是照抄。

## 五、渲染侧取数速查表（2026-09-23 校订，升级后复核）

| 需求数据 | 从哪取 |
|---|---|
| 相机 | `Camera` + `Projection` + `GlobalTransform`（清除色/视口/HDR 都在组件上） |
| 网格 | `Mesh3d(Handle<Mesh>)` → `Assets<Mesh>`（POSITION/NORMAL/TANGENT/UV_0/JOINTS_0/WEIGHTS_0 + `Indices`） |
| 材质 | `MeshMaterial3d<StandardMaterial>` → `Assets<StandardMaterial>`（base_color、metallic/roughness、`alpha_mode` 决定管线） |
| 贴图 | `Handle<Image>` → `Assets<Image>`（像素数据 + `TextureFormat` + sampler 设置，自建 VkImage） |
| 层级 | `GlobalTransform`（已传播）+ `ChildOf`/`Children` |
| 灯光 | PointLight / SpotLight / DirectionalLight；全局 `GlobalAmbientLight` 资源，相机 `AmbientLight` 组件可覆盖 |
| 蒙皮（主线暂不做） | SkinnedMesh 的关节列表与逆绑定矩阵资产、关节 GlobalTransform、顶点 JOINTS/WEIGHTS；自研侧仍需实现矩阵整理与顶点蒙皮 |
| 资产实例化 | bevy_gltf 产 WorldAsset，WorldAssetRoot 实例化为实体；不是旧 DynamicScene 路径 |

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
- **GPU-driven 内核（只读架构）**：在 `crates/bevy_pbr/src/render/` 查 mesh_preprocess、build_indirect_params、gpu_preprocess，在 `bevy_render/src/batching/` 查 GPU preprocessing；当前 shader 可能为 `.wesl`，按 checkout 定位，不固定旧后缀与行数
- **LOD 思路**：`examples/3d/visibility_range.rs`；遮挡：`examples/3d/occlusion_culling.rs`；meshlet：`examples/3d/meshlet.rs`

## 七、里程碑（建议顺序）

| 里程碑 | 内容 | 验证点 |
|---|---|---|
| M1 宿主壳 | 禁渲染 Bevy、窗口、ash 清屏 | 历史运行通过；新增 fence/present/退出生命周期缺陷须关闭 |
| M2 静态取数 | Assets 到 GPU 池、合批上传、descriptor indexing、直接绘制 | 统一不透明调试几何、base color/UV、光照方向分项对照，验证层实际启用 |
| M3 可靠增量 | 对象变更、资产事件、异步就绪、延迟回收、热重载 | 零变化零上传；换柄、同柄改内容、删除取消、反复重载正确；扫描成本另记 |
| M4 常驻 GPU-driven | 常驻实例表、GPU frustum、GPU indirect；共享资产与 chunk 组织 | 不回读可见集、不逐对象装配 draw；1万/5万/10万档测真实实例与成本 |
| M5 高级 GPU-driven 与流送 | 遮挡、GPU LOD、有预算的粗粒度流送 | 分项开关与持续游历压力验证；细 mip/LOD 流送、descriptor buffer 独立深化 |

详细段落、状态和性能预算以[路线图](学习目标实现步骤.md)为唯一执行表，不在入口重复维护第二套任务面板。

## 八、已知坑与注意

- 禁渲染宿主已实测；当前尚未关闭的实现问题集中在[缺陷文档](3-静态取数链路/材料/已实现缺陷与修复验收.md)，旧“收官”不覆盖新增复核。
- 资产未就绪时容忍空帧；不要将 CPU 有 Handle、GPU 上传完成、descriptor 可引用合成一个状态。
- `Changed` 是脏标记来源，不是 O(变化数) 的事件队列；只读路径避免不必要的可变解引用，资产内容另看 AssetEvent。
- 帧序为 Update 变更、PostUpdate 传播与采集、Last 提交；源码旧注释与执行冲突时以系统注册为准。
- Vulkan 的版本、supported feature、enabled feature、运行期有效用法分别检查；驱动返回成功不保证有效用法，探针也需验证层和控制变量。
- 浅克隆深查历史可能需要补历史；升级前先保存/审查本 fork 改动，在独立分支复核，不把直接 checkout 新 tag 当成完整升级流程。
- 本文学习事实锚点：`crates/bevy_asset/src/handle.rs`（强柄/Uuid）、`crates/bevy_asset/src/lib.rs`（watch 配置）、`crates/bevy_world_serialization/src/world_asset_spawner.rs`（重载重建）、`crates/bevy_mesh/src/skinning.rs` 与 `bevy_pbr/src/render/skin.rs`（姿态与蒙皮分工）。

## 九、文档分类归档规则（2026-09-22 定案）

> 全目录文档盘点后的定案：**新文档动笔前先按生命周期对号入座**，写完在文末登记；混血文档（设计文档里嵌教程、结果文档里嵌图谱）按主用途归一类。落位（2026-09-22 三次修订）：**根目录 = 总览层，平铺不设分类文件夹**——入口篇 + 路线图少而重要，平铺一眼可见；跨步骤专题笔记（宪法/方法论/工具层/侦察）集中 `笔记/`，不占根目录；**分类的主战场在 步骤文件夹内部**——步骤文件夹根放 README（材料目录）与该步主产出/主计划，其余专题文档（侦察/设计/知识点/手册）全进 `材料/`；施工段文件夹（N.M-*）照旧平级。
>
> ```text
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
- [3-静态取数链路/](3-静态取数链路/README.md)（M2：3.1 收官，3.2 进行中；2026-09-23 修订上传生命周期、WGSL 接口验证、reverse-Z 与受控对照，五段节奏不变）
- [已实现缺陷与修复验收](3-静态取数链路/材料/已实现缺陷与修复验收.md)（2026-09-23 代码复核：生产同步缺陷、timeline 探针有效性、证据等级与修复验收；仅文档，不宣称代码已修）

上游知识库（`C:\Users\zengkaixiang\.agents\docs\游戏制作\游戏引擎\渲染\渲染管线\`，绝对路径引用）：

- 《Bevy 大世界渲染选型与自研渲染器决策》——选型论证、决策清单、bevy_city 实操结论
- 《自研渲染器宿主引擎选型：Unity、Godot 与 Bevy》——三宿主对比与"练习价值/商业价值"拆账
- 《技术美术\Bindless 与 GPU 驱动渲染参考》——内核文件定位、官方 PR、生态项目、阅读顺序
