# 实体结构三视图：Blender大纲、glTF与Bevy ECS的对照与转换

底座：FlightHelmet（`assets/models/FlightHelmet/FlightHelmet.gltf`，Maya 2018 的 babylon.js 导出器产出、少量手工修改）。写作缘起：3.10 实体层级树窗口展示的树与 Blender 大纲"长得不一样"，本文把同一份资产在三个工具里的结构对齐，并重点记两条转换链：glTF→Blender、glTF→Bevy ECS。

## 0 判定线

- **三个视图是三种东西，不是同一结构的三种画法**：Blender 大纲是"数据块从属视图"，glTF 是"归一化仓库"（平行数组+索引引用），Bevy ECS 是"绘制单元树"。
- **glTF 才是把树拍平的那个。** Blender 导入时把数组索引引用"反归一化"回所有物层级；Bevy 加载时把 node 树立成实体、把 mesh+material 拼回 primitive 叶子实体。两边拼回来的观感相似（都是"节点下挂着东西"的树），但拼的是不同的东西：Blender 拼的是**所有物**（object 拥有 data、data 拥有材质槽），Bevy 拼的是**场景层级+绘制单元**。
- 判断任何"结构对不上"，先问一句：**看的是哪种关系？**Blender 树里混着两种关系（Object 父子、数据块从属）；Bevy 树里只有场景层级（`ChildOf`/`Children`）与组件；glTF 里只有数组与索引，层级深度由 node 树单独决定。

## 1 三个视图实测

### 1.1 Blender 大纲（5.2 导入视图）

```
Scene Collection
├─ GlassPlastic_low        ← Object
├─ Hose_low                ← Object
│  └─ Hose_low             ← 网格数据块（data）——不是子对象
│     └─ HoseMat           ← 材质槽（slot）——更不是子对象
├─ LeatherParts_low
├─ Lenses_low
├─ Light                   ← Blender 新建场景自带的默认灯，与文件无关
├─ MetalParts_low          → MetalParts_low ▸ MetalPartsMat
└─ RubberWood_low          → RubberWood_low ▸ RubberWoodMat
```

- 六个 Object 平铺在 Scene Collection 下——因为文件本身就是全平铺（见 1.2），Blender 忠实还原了这一点。
- **大纲的两种缩进语义不同**：Object 之间的父子（本文件没有）对应 glTF node 树；Object 之下的缩进是数据从属（网格数据块、材质槽），对应的是引用关系，不是场景层级。

### 1.2 glTF JSON（磁盘真身）

```
asset.generator = "babylon.js glTF exporter for Maya 2018 v20200228.3 (with minor hand modifications)"
scenes[0]           ：无 name，nodes = [0,1,2,3,4,5]
nodes[0..5]         ：仅 {name, mesh:i} 两个键——无 translation/rotation/scale/matrix，无 children
meshes[0..5]        ：每 mesh 恰 1 个 primitive；mesh i 的 primitive 引用 materials[i]（一一对应）
materials[0..5]     ：HoseMat / RubberWoodMat / GlassPlasticMat / MetalPartsMat / LeatherPartsMat / LensesMat
cameras             ：无；extensionsUsed：无（KHR_lights_punctual 未启用，文件无灯无相机）
textures/images     ：15 张 png = 5 材质 × 3（BaseColor / Normal / OcclusionRoughMetal 三合一打包图）
```

- **glTF 的层级只有 node 树这一棵**（本文件深度 1）；mesh/material/texture 永远是与 node 平行的数组，彼此只靠索引引用。
- OcclusionRoughMetal 三合一打包图被 occlusion 与 metallicRoughness 两个语义槽共用——这就是 3.1 采集"24 个贴图槽去重后 15 张"的那处重合。

### 1.3 Bevy ECS（0.20.0-dev 运行时主 World）

```
相机 #349 / 方向光 #350     ← 3.1.3 项目侧自己 spawn（文件里没有）
WorldAssetRoot #351        ← 进场壳：只持一个 Handle，等资产到货
└─ Scene0 #369             ← glTF scenes[0] 成为实体（scene 无名 → 兜底名 "Scene0"）
   ├─ Hose_low #370                ← node 实体：Transform + Visibility + Name
   │  └─ Hose_low.HoseMat #376     ← primitive 实体：Mesh3d + MeshMaterial3d + Aabb
   ├─ RubberWood_low #371          → RubberWood_low.RubberWoodMat #377
   ├─ GlassPlastic_low #372        → #378
   ├─ MetalParts_low #373          → #379
   ├─ LeatherParts_low #374        → #380
   └─ Lenses_low #375              → #381
```

- **材质在 Bevy 不是实体**：#377 这类 primitive 实体同时持 `Mesh3d`（网格柄）与 `MeshMaterial3d<StandardMaterial>`（材质柄），是"一个 draw call 的全部输入"。名字 `"{mesh 名}.{材质名}"` 来自 loader 的 `primitive_name`（`bevy_gltf/src/loader/gltf_ext/mesh.rs:10`）；本文件 mesh 与 node 同名（Maya 导出约定），所以看起来像"节点名.材质名"。
- 网格/材质/贴图本体活在 `Assets<T>` 容器里，实体只持柄——ECS 树管结构、资产容器管内容，这是 3.2/3.3 上传链的分工根基。
- 3.10 树窗口的 `[mesh] [mat]` 标 = 按组件存在性打标（`Mesh3d` / `MeshMaterial3d<StandardMaterial>`）。

## 2 转换链一：glTF → Blender（导入 = 反归一化）

Blender 导入器把归一化数组的索引引用翻译回"所有物"层级，逐项映射：

| glTF | Blender 落点 |
|---|---|
| node 树 | Object 父子层级（node 的 TRS → Object 变换；本文件全平铺，故大纲全是兄弟） |
| node.mesh 索引 → meshes[i] | Object 的 data 链接（网格数据块） |
| primitive.material 索引 → materials[i] | Object/网格的材质槽（每 primitive 的材质一个槽） |
| textures/images | 材质节点图里的 Image Texture 节点 |
| node.camera / KHR_lights_punctual | Camera / Light 类型 Object（本文件两者皆无） |

要点：大纲上"挂在一起"的东西，在 glTF 里隔着数组、只靠索引相连。Blender 的观感"Object→Data→材质"三层从属，是导入器重建出来的**所有物视图**，不是文件里的形状。

## 3 转换链二：glTF → Bevy ECS（两段式）

### 段 1：加载期——草稿 World 里组装（bevy_gltf loader，后台线程）

- 每个 glTF scene 开一个全新的 `World::default()` 草稿世界（`crates/bevy_gltf/src/loader/mod.rs:1049`）。
- 场景根实体：`Transform + Visibility + Name`，scene 无 name 时兜底 `"Scene{i}"`（`loader/mod.rs:1056-1066`）——"Scene0" 的来源。
- `load_node` 递归：node 实体挂 `Transform + Visibility + Name`（`loader/mod.rs:1560-1563`）；node 带相机则把 `Camera3d + Projection` 组件**插在 node 实体上**（`loader/mod.rs:1623`），带灯则 spawn **node 的子实体**挂 `DirectionalLight` 等（`loader/mod.rs:1781`）——两者落点不同。
- 每个 primitive 一个 node 子实体：`Mesh3d` + `Aabb`（`loader/mod.rs:1690-1695`），`Name = "{mesh 名}.{材质名}"`（`loader/mod.rs:1754`）。
- 材质挂接：官方干这事的 `GltfExtensionHandlerPbr` 是 `pub(crate)` 且随 PbrPlugin 被本项目禁用，故由自写 `AshMaterialHook::on_spawn_mesh_and_material` 按标签取柄插 `MeshMaterial3d`（`ash_renderer/src/scene/mechanism/material_hook.rs:106`）。
- 收口：`WorldAsset::new(world)` 把整棵草稿树打包成资产，以 `"Scene0"` 标签挂进 Gltf 资产（`loader/mod.rs:1149`）。

### 段 2：进场——反射展开进主 World

- 项目侧 Startup 只有两行：`server.load("…#Scene0")` + `spawn(WorldAssetRoot)`（`ash_renderer/src/scene/content/world_asset.rs:26`）；`WorldAssetRoot` 的 `Changed` 查询把柄排进 spawner 队列，资产未到货则每帧重试。
- 资产 `Added` 后 `world_instance_spawner_system` 把草稿 World 的组件经 `AppTypeRegistry` **反射写入**主 World 新实体，用 `EntityHashMap` 重映射全部实体引用（父子、蒙皮关节），再把草稿树顶层实体统一挂到 `WorldAssetRoot` 名下（`crates/bevy_world_serialization/src/world_asset_spawner.rs:565`）；未注册反射的类型在此 panic（`world_asset_spawner.rs:635`——material_hook 补 `register_asset_reflect` 的原因）。

### 概念落点表

| glTF 概念 | Bevy 落点 | 是实体吗 |
|---|---|---|
| scenes[i] | 场景根实体（Scene0，挂 WorldAssetRoot 之下） | 是 |
| nodes[i] | node 实体（Transform + Name，纯层级节点） | 是 |
| meshes[i] | `Assets<Mesh>` 的一行 | 否 |
| primitive | node 的子实体（`Mesh3d`+`MeshMaterial3d`+`Aabb`，一个 draw 的全部输入） | 是 |
| materials[i] | `Assets<StandardMaterial>` 的一行 | 否 |
| textures / images | `Assets<Image>` 的一行 | 否 |
| node.camera | node 实体上的相机组件 | 是（合在 node 上） |
| KHR lights | node 的子实体 | 是（下放一层） |

## 4 总对照表

| 概念 | Blender 大纲 | glTF JSON | Bevy ECS |
|---|---|---|---|
| 场景 | Scene / View Layer | scenes[] | Scene0 实体（WorldAssetRoot 之下） |
| 节点 | Object（父子层级） | nodes[]（唯一的一棵树） | node 实体（Transform+Name） |
| 网格 | Object Data（从属缩进） | meshes[]（索引引用） | `Assets<Mesh>` 行（非实体） |
| 材质 | 材质槽（从属缩进） | materials[]（索引引用） | `Assets<StandardMaterial>` 行（非实体） |
| 绘制单元 | 无此概念（object 即绘制） | primitive（mesh 内子结构） | primitive 子实体 |
| 贴图 | 材质节点图 Image Texture | textures[]/images[] | `Assets<Image>` 行（非实体） |
| 相机/灯 | Camera/Light 类型 Object | node 附加字段 / KHR 扩展 | 相机组件合在 node；灯是 node 子实体 |

## 5 钉子

1. **Blender 大纲的两种缩进语义不同**：Object 之间的父子才是场景层级（对应 node 树）；Object 之下的缩进是数据从属（data、材质槽），不是子对象。
2. **"网格实体/材质实体"在任何视图里都不存在**：Blender 里是数据块/槽位，glTF 里是数组行，Bevy 里是 `Assets<T>` 行。三视图里唯一带层级的只有 node 树。
3. **AssetId 分配序 ≠ glTF 数组序**：`Assets<StandardMaterial>` 的 0 号被 `AshMaterialHook::on_root` 预置的 DefaultMaterial 兜底占据，文件材质顺延（实例：RubberWoodMat 是 glTF `materials[1]`，AssetId 却是 2）；`Assets<Mesh>` 无兜底，AssetId 与 meshes[] 同序。
4. 实体号的连排（六 node #370~375、六 primitive #376~381）是草稿世界按批次展开的指纹；#351 与 #369 之间的空洞是加载期别处临时实体的痕迹——**实体号只反映分配序，不反映数量与结构**。
5. glTF 是交换中枢：本文件出自 Maya（Babylon 导出器），Blender 与 Bevy 都只是"视图"。比较结构时以 glTF 数组+索引为基准，别拿任一工具的树形当真身。

## 6 与本项目接口

- **3.10 树窗口**：只读展示主 World 实体树（`Children`/`ChildOf` 递归），`[mesh] [mat]` 标按组件存在性；点选产出 `SelectedEntity`，是 3.11 实体编辑的目标来源。
- **3.1.4 采集**：`CollectedScene` 采的就是 primitive 实体（6/6 带父链）——primitive 实体是渲染消费的最小单位。
- **3.11/3.12 编辑落点**：Transform 编辑落在 node/primitive 实体的 `Transform` 组件上，BRP 直控调同款实现。
- **柄即接口**：实体树只持 `Handle`/`AssetId`，内容全在资产容器——3.2/3.3 上传链只认 `AssetId<Mesh>`/贴图柄，与实体树解耦。
