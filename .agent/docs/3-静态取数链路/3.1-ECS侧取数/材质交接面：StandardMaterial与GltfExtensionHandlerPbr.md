# 材质交接面:StandardMaterial 与 GltfExtensionHandlerPbr 在数据链里的位置

> 2026-09-22。施工 3.1.1 的机制讲解篇,回答"这两个东西对我们项目有什么帮助、为什么要接入"。
> 实现细节、loader 侧调用契约(带行号)与验证证据见[《材质缝接线:自写AshMaterialHook三钩子》](材质缝接线：自写AshMaterialHook三钩子.md);glTF 加载五站全景见 步骤 1《glTF加载链路:从磁盘到Mesh3d.md》。

## §0 主线一句话

**Bevy 管"材质是什么"(语义与生命周期),我们管"材质怎么画"(GPU 表示);`StandardMaterial` 是两边的数据契约,`GltfExtensionHandlerPbr` 是把 glTF 翻译进这份契约的加载现场装配工。** 不接入的后果是静默缺料:glTF 场景照样 spawn,但每个 primitive 实体只有 `Mesh3d`、没有 `MeshMaterial3d`——采集系统(3.1.4)查到的是"知道形状、不知道穿什么"的裸网格,取数链路从实体侧断头。

## §1 为什么拆成两层:bevy_gltf 故意不产渲染语义

bevy_gltf 加载 glTF 时,`load_material` 把文件里的材质解析成 `GltfMaterial`——纯数据结构(base_color、metallic、perceptual_roughness、各贴图 handle……约 30 字段),不知道任何渲染器的存在。这是刻意解耦:bevy_gltf 不依赖任何渲染 crate(bevy_pbr 反向依赖它),所以禁渲染后 glTF 链路照常工作(步骤 1《DefaultPlugins分类》已核实)。

"中性数据 → 能渲染的材质"这一步,交给 `GltfExtensionHandler` 扩展缝:**渲染侧参与者在加载现场注册回调**,loader 每处理一个材质、每 spawn 一个 primitive 实体就回调一次。官方参与者 = `GltfExtensionHandlerPbr`(bevy_pbr/src/gltf.rs:102),由 PbrPlugin::build 经 `add_gltf`(:13)注册进 `GltfExtensionHandlers` 资源。

Unity 映射(机制先行,类比随后):`GltfMaterial` ≈ ModelImporter 解析出的原始材质数据;`GltfExtensionHandler` ≈ **AssetPostprocessor**(导入现场拦截、转换、赋给网格);`StandardMaterial` ≈ 转换产物(Standard shader 的材质资产)。

三钩子分工(官方只实现这三个,其余钩子走默认空实现):

| 钩子 | 调用时机 | 干什么 |
|---|---|---|
| `on_root` | 每文件一次 | 预置 `DefaultMaterial/std` 兜底——无材质 primitive 的落点 |
| `on_material` | 每材质 | `GltfMaterial` → `StandardMaterial`,以 `{label}/std` 入 `Assets<StandardMaterial>` |
| `on_spawn_mesh_and_material` | 每 primitive 实体 | 按 `{label}/std` 取 handle,插 `MeshMaterial3d(handle)`——**实体↔材质连接线** |

## §2 对项目的四个实际帮助

1. **白嫖整套转换**。glTF 的 metallic-roughness 打包、贴图通道、KHR 扩展(clearcoat/anisotropy/specular,loader 在 `load_material` 里内联解析,bevy_gltf/src/loader/mod.rs:1420-1428)——官方 pub 函数 `standard_material_from_gltf_material`(bevy_pbr/src/gltf.rs:33)连 feature-gate 的贴图通道一次全处理。自己维护一份映射表,bevy 加字段就漏;调用它,自写的只剩三钩子骨架。
2. **采集系统有了查询目标**。3.1.4 要 Query `Mesh3d + GlobalTransform + MeshMaterial3d`——第三项就是 handler 插上的。没有这一步,取数链路从实体侧断头。
3. **容器 = 资产生命周期机器**。`init_asset::<StandardMaterial>()` 之后,Assets<StandardMaterial> 白赚 handle 去重与依赖追踪(经 `get_label_handle` 建依赖边,与 Mesh3d 同机制——材质未就绪场景不算就绪)。本项目架构是"渲染器直读容器"(禁渲染后主世界数据永久可读,glTF 链路篇 §6 结论),施工 3.3/3.4 要从这里读 `base_color_texture` 的 Handle 换算成 bindless 槽位。
4. **A/B 对照同源**。判定线②要求与 bevy wgpu 同屏对照——两个渲染器读**同一份** StandardMaterial 数据,几何/贴图/光照方向的差异才能归因于渲染器实现本身,而不是"两边材质数据不一样"。

## §3 为什么是"自写复刻"这个接法

官方参与者进不来,是被两个事实锁死的:`GltfExtensionHandlerPbr` 是 `pub(crate)`(外部连 Box::new 都做不到),且其注册点在 PbrPlugin::build 内——PbrPlugin 在禁渲染宿主壳里必炸(步骤 2 结论,Assets<Shader> 无人注册)。

但这条缝本身是公开扩展点:`GltfExtensionHandler` trait 与 `GltfExtensionHandlers` 资源都是 pub,KHR 扩展走同一机制注册。所以我们做的事本质是**把"wgpu 渲染器参与者"换成"ash 渲染器参与者"**,翻译函数照用官方的。

不用"事后补丁"方案(场景加载完再自己遍历 Gltf 容器补插材质)的原因:handler 跑在 loader 官方时序里,标签体系(`{label}/std`)与 loader 的子资产寻址天然一致,还带 on_root 兜底语义;事后补丁要自己对 Gltf 容器做二次寻址、与场景展开时序对表,是非设计内路径。

## §4 边界:接的是数据半边,不是 wgpu 材质系统

- **接进来的**:StandardMaterial 的 CPU 数据半边——base_color、贴图 handle、unlit、alpha_mode、cull_mode……当"材质数据的规范容器"用。
- **不接的**:它的 GPU 半边(AsBindGroup bind group layout、MaterialPipeline、wgpu shader 那套)属于被禁的渲染栈——那半边正是我们的 bindless 描述符要替代的(施工 3.3/3.4)。
- 施工 3.3/3.4/3.5 实际取用:`base_color_texture`(Handle<Image> → VkImage → set0 槽位)、`base_color` 因子(push constant)、`unlit`/`alpha_mode`(3.5 按简单策略处理并记录取舍)。
- 生长点:将来若需要渲染器专属字段,在 AshMaterialHook 的 on_material 里对转换结果再加工即可——缝在我们手里。

## §5 一句总纲(复述锚点)

**handler 负责在加载现场把 glTF 翻译成标准语义(何时翻译:load 时,不是渲染时);StandardMaterial 是翻译产物与数据契约;容器让渲染器按需直读。Bevy 管语义与生命周期,我们管 GPU 表示——交接面就是这两个东西。**
