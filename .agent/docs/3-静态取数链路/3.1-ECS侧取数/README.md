# 施工 3.1：ECS 侧取数——材质缝接线与场景进场（零 Vulkan 代码）

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。

**目的**：先让数据在 ECS 里看得见——这是"取数链路"的取数半边，Vulkan 半边全部后置到施工 3.2 / 3.4。

**状态：🚧 施工中（2026-09-22 开工）——任务 3.1.1、3.1.2 已完成，3.1.3~3.1.4 待施工**

## 任务清单

- [x] **3.1.1 材质缝接线**（步骤 2 遗留）：`init_asset::<StandardMaterial>()` 注册容器；官方 `GltfExtensionHandlerPbr` 是 `pub(crate)` 拿不到，但转换函数 `standard_material_from_gltf_material` 是 pub——自写 AshMaterialHook 实现三钩子（on_root 兜底材质 / on_material 转换 / on_spawn_mesh_and_material 插 `MeshMaterial3d`），注册进 `GltfExtensionHandlers`；✅ 2026-09-22，见[《材质缝接线：自写AshMaterialHook三钩子》](3.1.1-材质缝接线：自写AshMaterialHook三钩子.md)（hook 实际触发验证随 3.1.2 场景进场兑现）
- [x] **3.1.2 场景进场**：Startup 里 `asset_server.load` FlightHelmet + spawn `WorldAssetRoot`；✅ 2026-09-22，见[《场景进场：一个load请求与三个禁渲染补位》](3.1.2-场景进场：一个load请求与三个禁渲染补位.md)（3.1.1 遗留的"hook 实际触发"已兑现：primitive 实体 6/6 带 MeshMaterial3d、6 材质 15 贴图入库）
- [ ] **3.1.3 相机与灯光**：手动 spawn 相机（`Camera` + `Projection` + `Transform`）与方向光/环境光（无 RenderPlugin，没人替我们建）；
- [ ] **3.1.4 采集系统 `collect_scene`**：挂 PostUpdate、`.after(TransformSystems::TransformPropagate)`；Query 采集 `Mesh3d`/`GlobalTransform`/`MeshMaterial3d`，`Assets::get` 容忍空帧（异步到货），日志报实体数/顶点数/材质数。

## 验证（本段完成标准）

1. 日志稳定报出 FlightHelmet 的实体数、每 primitive 顶点数、6 材质 15 贴图（文件 15 张 png 全被引用；容器 Image 17 = 内置 2 + 15）；
2. 无 panic，两 Tier 纪律（容忍空帧是正常态不是失败）；
3. 清屏循环不受影响（步骤 2 行为零回退）。

## 材料清单

- 《[材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md](材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md)》——任务 3.1.1 机制讲解篇（2026-09-22）：两层拆分的原因（bevy_gltf 故意不产渲染语义）、三钩子分工表、对项目的四个帮助（白嫖转换/采集查询目标/容器生命周期/A-B 同源）、为什么自写复刻、数据半边与 GPU 半边的边界
- 《[3.1.1-材质缝接线：自写AshMaterialHook三钩子.md](3.1.1-材质缝接线：自写AshMaterialHook三钩子.md)》——任务 3.1.1 小节记录（2026-09-22）：缝在哪里断（容器 + handler 双失守）、loader 侧调用契约（标签 `{label}/std`、on_root 兜底）、代码落点与 `gltf::` 名字歧义坑、验证证据、顺手清掉的 步骤 2 存量 clippy
- 《[3.1.2-场景进场：一个load请求与三个禁渲染补位.md](3.1.2-场景进场：一个load请求与三个禁渲染补位.md)》——任务 3.1.2 小节记录（2026-09-22）：进场链路与到货统计（兑现 3.1.1 hook 触发验证）、三个禁渲染补位（资产根路径 / ImageLoader 永久 Pending 卡死贴图加载 / MeshMaterial3d 反射注册 panic）、兜底材质不驻留容器的机制
- 《[场景三层含义：glTF scene、WorldAsset与引擎World.md](场景三层含义：glTF scene、WorldAsset与引擎World.md)》——术语澄清篇（2026-09-22，3.1.2 中用户提出"glTF 只是资源集合，与引擎里各元素堆砌的场景会不会混"）：三层 scene 对照表（glTF scenes[] / WorldAsset / 引擎 World + Unity 映射）、SceneRoot→WorldAssetRoot 改名证据（源码注释：scene 术语让给 bevy_scene）、本项目行文口径与速查判定

## 待决问题

- 相机位姿与朝向：FlightHelmet 无内嵌相机，手动 spawn 的初始参数施工中定。
