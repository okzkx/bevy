# glTF 加载链路：从磁盘到 Mesh3d

> 2026-09-20 建档，Q4 收尾篇（消费端半篇见 [BSN场景语法与Unity场景对比.md](BSN场景语法与Unity场景对比.md)）。行号基于 0.19.1。
> **结论先行：Bevy 与 frenderer 用的是同一个 `gltf` crate**——分野不在"怎么解析 glTF"，在解析完的数据进什么体系：frenderer 是"资产直通 GPU"的管道，Bevy 是"资产进中央仓库、渲染器自取"的解耦架构。自研渲染器正好站中间：借 Bevy 的仓库（M2 读 `Assets<Mesh>`），用自己的上传策略（M3 增量）。

## 1. 链路总览

```text
asset_server.load("model.gltf#Scene0")        ← 立即返回占位 Handle，数据异步到货
   │
   ├─ 异步任务：GltfLoader::load_gltf（loader/mod.rs:240）
   │    ├─ gltf crate 解析 JSON；load_buffers 读 bin/图片字节（:271）
   │    ├─ 逐 mesh 逐 primitive 构造 bevy_mesh::Mesh（:718-898）
   │    │     └─ add_labeled_asset("Mesh0.Primitive0", mesh)   子资产入库（:865）
   │    ├─ 逐材质构造 GltfMaterial（0.19：纯数据结构，渲染无关）（:1253）
   │    │     └─ PBR 扩展处理器把它转成 StandardMaterial 资产
   │    └─ 临时 World 里逐节点 spawn 实体树（:1512 起）
   │          └─ finish(WorldAsset::new(world))   整树序列化成资产（:1122）
   │
   ├─ 渲染语义注入：GltfExtensionHandlerPbr 往 mesh 实体插 MeshMaterial3d
   │
   ├─ 用户 spawn(WorldAssetRoot(handle)) → 组件 Add hook 展开整树进主 World
   │     └─ mesh 实体 = (Mesh3d(Handle<Mesh>), Transform, GlobalTransform, Aabb, …)
   │
   └─ 渲染系统 Query(&Mesh3d, &GlobalTransform) → Assets<Mesh> 读顶点     ← M2 起点
```

## 2. 第 0 站：注册（GltfPlugin）

`GltfPlugin::build`（`bevy_gltf/src/lib.rs:269-277`）做两件事：

- `init_asset` 注册 6 个资产类型：`Gltf`（容器）/`GltfNode`/`GltfMesh`/`GltfPrimitive`/`GltfSkin`/`GltfMaterial`；
- `preregister_asset_loader::<GltfLoader>(&["gltf", "glb"])` 按扩展名挂 loader。

另有一个 0.19 新机制：`GltfExtensionHandlers` 资源（扩展处理器列表），`bevy_pbr::add_gltf` 往里注册 `GltfExtensionHandlerPbr`（`bevy_pbr/src/gltf.rs:13-29`）——见 §5。

## 3. 第 1 站：请求（AssetServer + #label 寻址）

- `asset_server.load(GltfAssetLabel::Scene(0).from_asset("model.gltf"))`，等价字符串写法 `"model.gltf#Scene0"`（`label.rs:16-29` doc）；
- label 枚举：`Scene/Node/Mesh/Primitive{mesh,primitive}/Material/Animation`（`label.rs:33-90`）——**label 是子资产寻址键**，"路径#label" 全局唯一；
- load 立即返回占位 Handle（数据异步到货，已知机制）；同路径同 label 自动去重，复用同一 Handle。

## 4. 第 2 站：解析（异步任务里的 load_gltf）

### 4.1 Mesh 转换（:718-898，逐 mesh 逐 primitive）

- `Mesh::new(topology, settings.load_meshes)`（:755）——第二参是 `RenderAssetUsages`（见 §6），**加载时就声明这份数据给谁用**；
- 顶点按语义**分列**存储：`convert_attribute` → `mesh.insert_attribute(attribute, values)`（:771-780），POSITION/NORMAL/UV 各一列，不是交错布局（frenderer 是固定 7 属性交错，见 §7）；
- 索引 `insert_indices`，U8 自动升格 U16（:786-792）；
- morph targets 原样搬进 `PrimitiveMorphAttributes`（:794-815）；
- **补全按需而非无条件**：缺法线 → `duplicate_vertices` + `compute_flat_normals`（:819-836）；材质需要切线且没有 → mikktspace 生成（:838-856）——frenderer 是无条件强制 mikktspace（`mesh_builder.rs:71-77`）；
- `add_labeled_asset("Mesh0.Primitive0", mesh)`（:865）注册为子资产，一个 glTF **primitive = 一个 Mesh 资产**。

### 4.2 材质：0.19 的解耦变化（重点）

**`bevy_gltf` 里已经没有 `StandardMaterial`**。`load_material`（:1253）产出的是渲染无关的纯数据结构 `GltfMaterial`（base_color/metallic/roughness/贴图 Handle，`material.rs:12` doc 明说对位 StandardMaterial）——数据/渲染 crate 解耦贯彻到了材质层。转换职责移交给扩展处理器（§5）。

### 4.3 场景树与 WorldAsset（:1512 起，load_node）

临时 World 里逐 glTF 节点 spawn 实体：节点 Transform + `ChildOf`/`Children` 层级；每个 primitive 追加一个子实体，核心组件（:1669-1674）：

```rust
let mut mesh_entity = parent.spawn((
    Mesh3d(load_context.get_label_handle(primitive_label.to_string())),  // :1671
    mesh_entity_transform,
));
```

- 注意是 `get_label_handle`（不是 add）：**场景资产对 Mesh 子资产建依赖边**——Mesh 没就绪，场景就不算就绪（依赖系统结算后才发 `AssetEvent::LoadedWithDependencies`）；
- 随后按内容补 `Aabb`（:1705，来自 glTF bounds）、`GltfMeshName`/`GltfMaterialName`/`GltfExtras`、相机/灯光/动画/蒙皮等；
- 每个场景 `finish(WorldAsset::new(world))`（:1122）→ 整棵实体树序列化成 `WorldAsset` 资产；
- 最终返回 `Gltf` 容器（:1134-1158）= 全部子资产 Handle 的目录（scenes/meshes/materials/nodes/animations + named_* 两个视图）。

## 5. 第 3 站：渲染语义注入（GltfExtensionHandler）

`bevy_pbr/src/gltf.rs:104-163` 的 `GltfExtensionHandlerPbr`，三个钩子：

| 钩子 | 做什么 |
|---|---|
| `on_root`（:108） | 给 glTF DefaultMaterial 也造一份 StandardMaterial（缺材质兜底） |
| `on_material`（:124） | `standard_material_from_gltf_material`（:33）把 GltfMaterial 转成 StandardMaterial 资产，label = `"Material0/std"` |
| `on_spawn_mesh_and_material`（:140） | 往 mesh 实体 `insert(MeshMaterial3d(handle))`（:152-160） |

架构意义：**loader 产出中立数据，渲染 crate 决定渲染语义**。本项目 PbrPlugin 保留（已核实优雅降级），StandardMaterial 照常生成；将来可写自己的 handler 把材质转成 bindless 池想要的形态——**这个扩展点就是自研渲染器的接入缝**。

## 6. 第 4 站：实例化与取数

- `spawn(WorldAssetRoot(handle))`（`bevy_world_serialization/src/components.rs:23`）；
- 组件 Add hook（`lib.rs:96-100`）触发异步加载，依赖就绪后整树展开进主 World（走 SpawnScene——已核实的 Update/PostUpdate 之间时序）；
- 节点层级 ChildOf/Children，PostUpdate 变换传播出 `GlobalTransform`；
- **渲染器取数（M2 起点）**：`Query<(&Mesh3d, &GlobalTransform)>` → `Assets<Mesh>` 读顶点 → 上传 ash。

### RenderAssetUsages：声明式"数据给谁用"

`bevy_asset/src/render_asset.rs:34-50`，位标志 `MAIN_WORLD = 1<<0` / `RENDER_WORLD = 1<<1`，默认 `MAIN_WORLD | RENDER_WORLD`：

- 官方 RenderPlugin 流程：extract 把资产搬到 RenderApp 后**释放主世界份数**（RENDER_WORLD-only 时主世界只剩空壳）；
- **本项目禁了 RenderPlugin：没人搬也没人释放，`Assets<Mesh>` 顶点数据永远在主世界**，直接读；想省一份内存可配 `MAIN_WORLD` only；
- 概念可抄：这就是"CPU 仓库驻留 vs GPU 驻留"的声明式版本，对应你池子的驻留决策；Unity 对应 mesh 的 Read/Write Enabled。

## 7. 对照 frenderer（同链路逐站）

frenderer（`F:\okzkx\rust-frenderer`）链路：

```text
FBX ──外部 FBX2glTF.exe──▶ glb（按文件哈希缓存，gen_glb.rs:9-23）
   └─ 同一个 gltf crate 解析 → 自有 Model/Mesh/Primitive 树
        ├─ 固定 7 属性交错 Vertex（vertex.rs:7-15），无条件 mikktspace（mesh_builder.rs:71-77）
        └─ thread_spawn + mpsc + LoadingObject 状态机，每帧 try_recv 收货
             └─ 上传在 main_mutex 下逐 primitive 立即 staging→DEVICE_LOCAL
                （每 buffer 独立 allocate_memory + 一次单发提交，无合批无 allocator）
   └─ 绘制：每帧现场收集可见 primitive 成 RenderElement（裸 vk::Buffer + index_count）
      → 排序 → 逐个 bind pipeline + vertex/index + descriptor set
```

| 维度 | Bevy | frenderer |
|---|---|---|
| 解析器 | gltf crate | gltf crate——同一把刀 |
| 顶点布局 | 按语义分列 | 固定 7 属性交错 |
| 补全逻辑 | 按需（缺才算） | 无条件全算 |
| 数据归宿 | Assets 中央仓库，渲染器来取 | 结构体直通 GPU buffer |
| 资产身份 | Handle/AssetId（可比、可进组件、强弱句柄） | Vec 下标 + 裸 vk 句柄 |
| 去重 | AssetServer 按 path#label 白送 | 无，同 glb 两引用 = 两份 VRAM |
| 上传时机 | **不做**，留给渲染器（本项目 M2/M3） | 加载线程里立即做（main_mutex 串行卡主线程） |
| 场景 | WorldAsset → ECS 实体树 | `*mut RenderObject` 裸指针树（render_object/mod.rs:119-138） |
| 热重载 | watch + AssetEvent，粒度=单个资产 | notify + 整模型 start_load 重载（shader 懒重建 device_wait_idle） |
| 卸载 | 强句柄计数自然回收 | 逐资源 Drop 里 `device_wait_idle` |

**frenderer 最薄弱的一环 = 资源管理层**：无句柄/引用计数/去重；上传在互斥锁下逐 buffer wait_idle，每个 drop 一次全局 wait_idle；规模一大就是性能与内存的双重瓶颈。反过来看，Bevy 白送的 AssetServer 去重 + Handle 体系，正是本项目已定的"bindless 池 key 用 AssetId、强句柄=驻留策略"的机制证据。

## 8. Unity 映射

| Bevy | Unity |
|---|---|
| `AssetServer.load("m.gltf#Mesh0")` | Addressables 加载 + sub-asset 寻址（或 ModelImporter 子资源） |
| `Gltf` 容器资产 | 导入后的 .fbx Model（含子 Mesh/Material） |
| `Mesh3d` 组件（`bevy_mesh/src/components.rs:102`） | `MeshFilter` |
| `MeshMaterial3d`（`bevy_pbr/src/mesh_material.rs:41`） | `MeshRenderer` + material 引用 |
| `WorldAssetRoot` | `Instantiate(prefab)` |
| `RenderAssetUsages` | mesh 的 Read/Write Enabled（markNoLongerReadable 的反向） |
| `GltfExtensionHandler` | **AssetPostprocessor**（导入管线钩子，改导入结果）——同位类比 |

## 9. 项目落点

1. **解析层不用重写**：GltfLoader 已把 gltf crate 解析成 `Mesh`+`GltfMaterial` 资产，M2 只需 Query → `Assets<Mesh>` 读数，解析站白送。
2. **数据一定在**：默认 RenderAssetUsages 下没人释放主世界数据（RenderPlugin 被禁），M2 直接读；显式配 `MAIN_WORLD` only 更省。
3. **材质接入缝**：留着 PbrPlugin 拿 StandardMaterial 玩 A/B 对照；自研材质走自己的 `GltfExtensionHandler`（或直接从 `GltfMaterial` 数据转换）。
4. **粒度提醒**：一个 glTF primitive = 一个 Mesh 资产 = 一个 Mesh3d 实体，大场景实体数放大——M4 `merge_car_meshes` 的用武之地。
5. **M3 增量上传的地基已现成**：Handle 强弱 = 驻留策略，`Changed<Mesh>`/`AssetEvent` = 脏标记，正是 frenderer "整模型重载 + 全局 wait_idle" 的超越点。
