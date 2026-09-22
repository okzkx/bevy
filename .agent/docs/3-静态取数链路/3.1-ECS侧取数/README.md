# 施工 3.1：ECS 侧取数——材质缝接线与场景进场（零 Vulkan 代码）

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。

**目的**：先让数据在 ECS 里看得见——这是"取数链路"的取数半边，Vulkan 半边全部后置到施工 3.2 / 3.4。

**状态：✅ 已完成（2026-09-22）——3.1.1~3.1.4 全部收官；ECS 侧"数据看得见"落地，GPU 半边自施工 3.2 起步**

## 任务清单

- [x] **3.1.1 材质缝接线**（步骤 2 遗留）：`init_asset::<StandardMaterial>()` 注册容器；官方 `GltfExtensionHandlerPbr` 是 `pub(crate)` 拿不到，但转换函数 `standard_material_from_gltf_material` 是 pub——自写 AshMaterialHook 实现三钩子（on_root 兜底材质 / on_material 转换 / on_spawn_mesh_and_material 插 `MeshMaterial3d`），注册进 `GltfExtensionHandlers`；✅ 2026-09-22，见[《材质缝接线：自写AshMaterialHook三钩子》](3.1.1-材质缝接线：自写AshMaterialHook三钩子.md)（hook 实际触发验证随 3.1.2 场景进场兑现）
- [x] **3.1.2 场景进场**：Startup 里 `asset_server.load` FlightHelmet + spawn `WorldAssetRoot`；✅ 2026-09-22，见[《场景进场：一个load请求与三个禁渲染补位》](3.1.2-场景进场：一个load请求与三个禁渲染补位.md)（3.1.1 遗留的"hook 实际触发"已兑现：primitive 实体 6/6 带 MeshMaterial3d、6 材质 15 贴图入库）
- [x] **3.1.3 相机与灯光**：手动 spawn 相机（`Camera` + `Projection` + `Transform`）与方向光/环境光（无 RenderPlugin，没人替我们建）；✅ 2026-09-22，见[《相机与灯光：引擎层自建与宽高比第四补位》](3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md)（朝向偏差 0.0000°、宽高比 1.7778 跟随主窗口、GlobalAmbientLight 在位；环境光真身订正为 GlobalAmbientLight 资源——AmbientLight 在 0.20 已是相机组件）
- [x] **3.1.4 采集系统 `collect_scene`**：挂 PostUpdate、`.after(TransformSystems::Propagate)`（0.20.0-dev 实名，旧文写的 `TransformPropagate` 已订正）；Query 采集 `Mesh3d`/`GlobalTransform`/`MeshMaterial3d`，`Assets::get` 容忍空帧（异步到货），产 `CollectedScene` 拓扑快照（3.2 上传 / 3.4 DrawList 的种子），日志报实体数/顶点数/材质数；✅ 2026-09-22，见[《采集系统：PostUpdate帧末直读与CollectedScene快照》](3.1.4-采集系统：PostUpdate帧末直读与CollectedScene快照.md)（primitive 6/6 带父链 6/6、顶点合计 55392 / 索引 284166、6 材质贴图槽 24 去重 15、传播契约 0.0000°）

## 验证（本段完成标准）

1. 日志稳定报出 FlightHelmet 的实体数、每 primitive 顶点数、6 材质 15 贴图（文件 15 张 png 全被引用；容器 Image 17 = 内置 2 + 15）；
2. 无 panic，两 Tier 纪律（容忍空帧是正常态不是失败）；
3. 清屏循环不受影响（步骤 2 行为零回退）。

## 材料清单

- 《[材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md](材质交接面：StandardMaterial与GltfExtensionHandlerPbr.md)》——任务 3.1.1 机制讲解篇（2026-09-22）：两层拆分的原因（bevy_gltf 故意不产渲染语义）、三钩子分工表、对项目的四个帮助（白嫖转换/采集查询目标/容器生命周期/A-B 同源）、为什么自写复刻、数据半边与 GPU 半边的边界
- 《[3.1.1-材质缝接线：自写AshMaterialHook三钩子.md](3.1.1-材质缝接线：自写AshMaterialHook三钩子.md)》——任务 3.1.1 小节记录（2026-09-22）：缝在哪里断（容器 + handler 双失守）、loader 侧调用契约（标签 `{label}/std`、on_root 兜底）、代码落点与 `gltf::` 名字歧义坑、验证证据、顺手清掉的 步骤 2 存量 clippy
- 《[3.1.2-场景进场：一个load请求与三个禁渲染补位.md](3.1.2-场景进场：一个load请求与三个禁渲染补位.md)》——任务 3.1.2 小节记录（2026-09-22）：进场链路与到货统计（兑现 3.1.1 hook 触发验证）、三个禁渲染补位（资产根路径 / ImageLoader 永久 Pending 卡死贴图加载 / MeshMaterial3d 反射注册 panic）、兜底材质不驻留容器的机制
- 《[场景三层含义：glTF scene、WorldAsset与引擎World.md](场景三层含义：glTF scene、WorldAsset与引擎World.md)》——术语澄清篇（2026-09-22，3.1.2 中用户提出"glTF 只是资源集合，与引擎里各元素堆砌的场景会不会混"）：三层 scene 对照表（glTF scenes[] / WorldAsset / 引擎 World + Unity 映射）、SceneRoot→WorldAssetRoot 改名证据（源码注释：scene 术语让给 bevy_scene）、本项目行文口径与速查判定
- 《[3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md](3.1.3-相机与灯光：引擎层自建与宽高比第四补位.md)》——任务 3.1.3 小节记录（2026-09-22）：引擎搭台不代建演员（Camera/DirectionalLight 实体从来是场景代码的事）、裸 Camera 不用 Camera3d 的理由、环境光真身订正（GlobalAmbientLight 资源 vs AmbientLight 组件）、第四补位宽高比（camera_system 在渲染族，禁后归我们）、官方示例同源取景为 3.5 对照打底
- 《[3.1.4-采集系统：PostUpdate帧末直读与CollectedScene快照.md](3.1.4-采集系统：PostUpdate帧末直读与CollectedScene快照.md)》——任务 3.1.4 小节记录，3.1 段收官篇（2026-09-22）：官方 Extract 缺位与帧末直读时机（PostUpdate、传播链之后；TransformSystems::Propagate / to_matrix 两处 API 订正）、快照只带拓扑不带内容的取舍、空帧容忍与传播契约（FlightHelmet 全恒等的诚实边界；贴图槽 24 去重 15 与容器 Image 17 互证）、代码落点与验证证据
- 《[代码结构整理：scene拆组与src分层.md](../材料/代码结构整理：scene拆组与src分层.md)》——3.1.3 收尾整理（2026-09-22，3.1.4 之前）：scene.rs 按职责五拆成 scene/ 子模块（mod.rs = 组清单 + 重出口，含相机/灯光解耦成组的代码追平）、vulkan 三件归拢 vulkan/（context/swapchain/frames）；main 与宿主桥不动；3.1.4 collect 照子模块落位

## 待决问题

- ~~相机位姿与朝向~~：已按官方 FlightHelmet 示例取景——(0.7, 0.7, 1.0) 看向 (0, 0.3, 0)，方向光 ZYX 欧拉 (0, -0.15π, -0.15π)、20000 lx（见 3.1.3 §2，3.5 同屏对照同源复用）
