# 施工 3.1：ECS 侧取数——材质缝接线与场景进场（零 Vulkan 代码）

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。

**目的**：先让数据在 ECS 里看得见——这是"取数链路"的取数半边，Vulkan 半边全部后置到施工 3.2 / 3.4。

**状态：🚧 施工中（2026-09-22 开工）——任务 3.1.1 已完成，3.1.2~3.1.4 待施工**

## 任务清单

- [x] **3.1.1 材质缝接线**（步骤 2 遗留）：`init_asset::<StandardMaterial>()` 注册容器；官方 `GltfExtensionHandlerPbr` 是 `pub(crate)` 拿不到，但转换函数 `standard_material_from_gltf_material` 是 pub——自写 AshMaterialHook 实现三钩子（on_root 兜底材质 / on_material 转换 / on_spawn_mesh_and_material 插 `MeshMaterial3d`），注册进 `GltfExtensionHandlers`；✅ 2026-09-22，见[《材质缝接线：自写AshMaterialHook三钩子》](材质缝接线：自写AshMaterialHook三钩子.md)（hook 实际触发验证随 3.1.2 场景进场兑现）
- [ ] **3.1.2 场景进场**：Startup 里 `asset_server.load` FlightHelmet + spawn `WorldAssetRoot`；
- [ ] **3.1.3 相机与灯光**：手动 spawn 相机（`Camera` + `Projection` + `Transform`）与方向光/环境光（无 RenderPlugin，没人替我们建）；
- [ ] **3.1.4 采集系统 `collect_scene`**：挂 PostUpdate、`.after(TransformSystems::TransformPropagate)`；Query 采集 `Mesh3d`/`GlobalTransform`/`MeshMaterial3d`，`Assets::get` 容忍空帧（异步到货），日志报实体数/顶点数/材质数。

## 验证（本段完成标准）

1. 日志稳定报出 FlightHelmet 的实体数、每 primitive 顶点数、4 材质 5 贴图；
2. 无 panic，两 Tier 纪律（容忍空帧是正常态不是失败）；
3. 清屏循环不受影响（步骤 2 行为零回退）。

## 材料清单

- 《[材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md](材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md)》——任务 3.1.1 机制讲解篇（2026-09-22）：两层拆分的原因（bevy_gltf 故意不产渲染语义）、三钩子分工表、对项目的四个帮助（白嫖转换/采集查询目标/容器生命周期/A-B 同源）、为什么自写复刻、数据半边与 GPU 半边的边界
- 《[材质缝接线：自写AshMaterialHook三钩子.md](材质缝接线：自写AshMaterialHook三钩子.md)》——任务 3.1.1 小节记录（2026-09-22）：缝在哪里断（容器 + handler 双失守）、loader 侧调用契约（标签 `{label}/std`、on_root 兜底）、代码落点与 `gltf::` 名字歧义坑、验证证据、顺手清掉的 步骤 2 存量 clippy

## 待决问题

- 相机位姿与朝向：FlightHelmet 无内嵌相机，手动 spawn 的初始参数施工中定。
