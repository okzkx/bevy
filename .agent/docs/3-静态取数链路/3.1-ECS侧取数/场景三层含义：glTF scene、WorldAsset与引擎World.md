# 场景三层含义：glTF scene、WorldAsset与引擎World

> 2026-09-22 建档，施工 3.1.2 中用户提出："平常说的场景是引擎里各元素堆砌的场景，glTF 只是资源集合，会不会有误解"——会，术语确实撞名。本文立三层口径，作为 3.1 起行文与后续文档的"场景"用词基准。

## §0 主线一句话

**"场景"在本项目有三个层面，遇到先问是哪一层**：glTF 文件里的 `scenes[]` = 资产自带的组装说明；`WorldAsset` = 它序列化后的资产形态；**引擎场景（World）= 各元素堆砌的运行时容器**。3.1.2 的"场景进场"是**资产实例化动作**，被填充的容器是引擎 World。

## §1 三层对照表

| 层 | 存在于 | 是什么 | Unity 对应 |
|---|---|---|---|
| glTF `scenes[]` | 资产文件内 | DCC 导出时带出来的节点组织：有哪些根节点、推荐的组装（层级 + transform 一起走） | prefab 资产**内部**的根层级 |
| `WorldAsset`（子资产 `…#Scene0`） | Bevy 资产系统 | 上者序列化后的资产形态：整棵实体树的存档 | prefab 资产本体 |
| **引擎场景 = World** | 运行时 | **全量数据库**：所有实体 + 所有资源（含 Vulkan 资源、Time、窗口等非渲染物） | Unity 的整个运行时状态（Scene 们 + 全局管理器） |
| 场景内容（渲染器语境） | World 的子集 | **会被渲染的实体集合**：资产实例（Mesh3d/MeshMaterial3d/GlobalTransform）+ 相机 + 灯光，按组件 Query 划定 | Unity 的 Scene 内容（会被画的 GameObject） |

## §2 glTF 文件里为什么会有 "scene"

DCC（Blender 等）导出时，场景层级跟着资产走——glTF 的 scene 不是"引擎场景"，是**这个文件自带的组装说明**。FlightHelmet：1 个 scene 挂 6 个根节点；一个文件也可以写多个 scene（多套推荐组装）。所以 `load("…#Scene0")` = 按 DCC 给的默认组装实例化，`GltfAssetLabel::Scene` 的 "Scene" 字样来自 glTF JSON 字段名，指资产内组织。

## §3 Bevy 侧类型现状（0.20.0-dev）

- 序列化实体树资产：`WorldAsset` / 实例化组件 `WorldAssetRoot`（bevy_world_serialization/src/components.rs:23）。**曾用名 `SceneRoot`**——源码注释原话（components.rs:15）："renamed from `SceneRoot`, in the interest of giving 'scene' terminology to Bevy's next generation scene system, available in `bevy_scene`"。即引擎自己把 "scene" 一词让给了书写语法侧，资产侧改名 WorldAsset 消歧；**读旧版教程资料时 Scene/SceneRoot/DynamicScene 对应今天的 WorldAsset/WorldAssetRoot/DynamicWorld**。
- `bevy_scene` 现在承载 BSN 场景书写语法（bsn! 宏、ScenePatch，见 步骤 1《BSN场景语法与Unity场景对比》）。
- 实例化行为：spawn `WorldAssetRoot` 后该实体带 `WorldInstance` 组件，且 `#[require(Transform)]`/`#[require(Visibility)]`——根实体即普通实体（≈ Instantiate 出来的 prefab 根）。

## §4 本项目的口径

- **3.1.2 场景进场 = 资产实例化动作**：loader 白送层级（节点 Transform + ChildOf/Children + primitive 实体 Mesh3d/MeshMaterial3d），进 ECS 的都是实体；引擎 World 是被填充的容器。
- 引擎场景的完整形态（3.5 收官时）= 头盔实例 + 相机 + 灯光。**相机/灯光不来自任何资产**（FlightHelmet 无内嵌相机灯光），3.1.3 手动 spawn——纯引擎层元素。
- "摆元素/搭场景"这类引擎层动作，在本项目对应 spawn + Transform，不是 load。

## §5 速查判定（2026-09-22 用户定案：跟随官方，弃 Scene 措辞）

**文件里的都是资产，进 World 的都是实体。**行文三分法：

| 指什么 | 用词 | 弃用 |
|---|---|---|
| 资产形态 | **WorldAsset / WorldAssetRoot** | "Scene 资产 / 场景资产 / 场景子资产" |
| glTF 文件内组织 | glTF `scenes[]` 字段 / `#Scene0` 标签 | "glTF 场景"（含糊） |
| 运行时容器 | **引擎场景 / World** | — |

两个**改不掉也不必改**的 Scene 残留：`GltfAssetLabel::Scene(0)` 枚举名与标签串 `#Scene0`——它们是 glTF JSON 字段名 `scenes[]` 的直译、资产寻址键的一部分，Bevy 侧同样保留；写代码遇到按"标签串"理解即可。

本项目的 `scene` 标识符家族（`scene.rs` 模块 / `SceneEntryPlugin` / `report_scene_arrival` / `collect_scene`）取**上表第四行义**——渲染器语境的"场景内容"（World 中会被渲染的子集），**不是 World 全量**：collect_scene 按 Query 只采 Mesh3d/MeshMaterial3d/GlobalTransform（及相机/灯光），不碰窗口与 Vulkan 资源，叫 world 反而名不副实；与被弃名的资产侧 Scene 也无关，不随 WorldAsset 改名。"场景"在这里是渲染器行话（scene = 一帧要画的内容集合），非 Bevy 术语。

## §6 复述锚点

1. glTF 的 scene 是资产自带的组装说明，不是引擎场景——
2. WorldAsset 是它的资产形态（旧名 Scene/SceneRoot），WorldAssetRoot ≈ Instantiate——
3. World 是全量数据库，"场景内容"是其中会被渲染的子集（实例+相机+灯光）；scene.rs 的 scene = 场景内容，不是 World 换名——
