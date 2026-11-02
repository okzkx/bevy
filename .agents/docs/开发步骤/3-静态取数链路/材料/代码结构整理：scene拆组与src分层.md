# 代码结构整理：scene 拆组与 src 分层

> 2026-09-22，3.1.3 收尾整理（3.1.4 之前）。动机：scene.rs 一个文件 381 行攘括了材质缝接线、场景进场、相机、灯光、公共工具四类职责——3.1.4 采集系统、3.2 起的取数件还要继续往里长；src 顶层 9 个文件平铺，Vulkan 三件与 bevy 侧文件混在一层，层次要靠文件名自己猜。
> 用户定案：**scene 按职责拆分（工具方法 / 业务场景搭建 / 资源加载）；src 代码文件分层。**

## §0 判定线

| 项 | 状态 |
|---|---|
| scene.rs 消失，职责各归子模块（mod.rs 只留组清单与重出口） | ✅ |
| main.rs 零改动（import 形状 `scene::{AshMaterialHookPlugin, SceneEntryPlugin}` 不变，冻结纪律不破） | ✅ |
| host.rs 仍单文件（生命周期是一根线，同日上一次整理的既定裁决不改） | ✅ |
| 行为零回退：clippy `-D warnings` 全净 + 实跑 10s 判定线数值逐字复现（0.0000° / 1.7778 / 6/6 / Image 17），零 WARN/panic | ✅ |

## §1 终态：src 分层地图

| 层 | 文件 | 职责 |
|---|---|---|
| 统筹 | `main.rs` | 插件组装（冻结） |
| 统筹 | `lib.rs` | 模块地图与分层声明 |
| 编排 | `host.rs` | 宿主桥：init/draw_frame/teardown 三系统 + 禁渲染补位 |
| Vulkan 资源 | `vulkan/context.rs` | 进程级：Entry/Instance/Surface/Device/Queue（原 vulkan.rs） |
| Vulkan 资源 | `vulkan/swapchain.rs` | resize 级：swapchain + images/views，重建幂等 |
| Vulkan 资源 | `vulkan/frames.rs` | 帧级：命令缓冲 + 双信号量 + fence 轮转 |
| ECS 数据 | `scene/material_hook.rs` | 材质缝接线（3.1.1）：容器 + 自写三钩子 + 缝自检 |
| ECS 数据 | `scene/world_asset.rs` | 场景进场（3.1.2）：load FlightHelmet + 到货统计 |
| ECS 数据 | `scene/camera.rs` | 相机组（3.1.3）：三件套 spawn + 宽高比补位 + 就位核验 |
| ECS 数据 | `scene/lights.rs` | 灯光组（3.1.3）：方向光 spawn + 环境光资源核验 |
| ECS 数据 | `scene/util.rs` | 子模块公共小工具（fmt_vec3） |
| 基础设施 | `error.rs` / `syntax.rs` | 类型化错误 / 语法糖 |

两个 mod.rs 都只干两件事：

- **组清单**：`SceneEntryPlugin` 的系统清单里一个组一行（"一个元素 = 一个 spawn + 一个就位核验（+ 专属修正）"，任意搭场景 = 清单里增删组）；
- **重出口**：子模块私有、类型重出口（`pub use material_hook::AshMaterialHookPlugin` / `pub use context::Context` 等）——main 与宿主桥的 import 路径一个没变。

对外可见的类型路径（`ash_renderer::scene::AshMaterialHookPlugin` 等）全部照旧；**变的是日志 target**（= 模块路径）：scene 四件从 `ash_renderer::scene` 长成 `ash_renderer::scene::material_hook / world_asset / camera / lights`，Vulkan 三件从 `ash_renderer::vulkan / swapchain / frames` 变成 `ash_renderer::vulkan::context / swapchain / frames`。历史施工文档里的日志证据是当时的实况，不回改。

## §2 迁移清单

| 动作 | 内容 |
|---|---|
| 新增 `scene/` 五子模块 | material_hook（缝接线 + 缝的背景故事注释）、world_asset（FLIGHT_HELMET + load + 到货统计）、camera（CAMERA_POS/TARGET 常量 + spawn + 宽高比补位 + 就位核验）、lights（方向光 spawn + 环境光核验）、util（fmt_vec3）；正文逐字迁，仅跨模块指引与可见性（pub(super)）调整 |
| 落实解耦成组 | 拆分时把"相机与灯光合在一个方法"的中间态一并拆开（用户 3.1.3 定案）：spawn_camera / spawn_lights 各自独立，组间零耦合——文档已按定案写好，本次代码追平 |
| 新增 `vulkan/` | vulkan.rs → vulkan/context.rs；swapchain.rs、frames.rs git mv 入 vulkan/；mod.rs 重出口，类型路径保持 `crate::vulkan::*` |
| 改 `host.rs` | import 收拢为 `crate::vulkan::{...}`（消费重出口）；模块 doc 跨模块指引同步 |
| 改 `lib.rs` | `pub mod frames/swapchain` 撤除（并入 vulkan/）；模块地图改写为分层视图 |
| 删 `scene.rs` | 内容已全部迁入 scene/ |

## §3 给 3.1.4 与 3.2~3.5 的边界约定

1. **取数件照子模块落位**：3.1.4 采集系统 = `scene/collect.rs` 新子模块 + mod.rs 组清单加一行，不再回大杂院；
2. **Vulkan 件照生命周期落位**：3.2 资源池（内存 helper + 顶点/索引池）、3.3 描述符、3.4 管线各自新子模块 + mod.rs 重出口，host 只加编排行——延续《代码结构整理：main只做统筹（宿主桥分家）》§3 的约定；
3. **每文件一件事**：新文件长出第二种职责即拆；拆分成本低，合并心智成本高。

## §4 验证证据（2026-09-22）

- `cargo clippy -p ash_renderer --all-targets -- -D warnings`：通过（拆分暴露 4 处 private_interfaces——三个"报一次即歇"状态结构体随系统签名升 pub(super)，语义不变）；
- 实跑 10s（debug，零 WARN / panic / VK-ERROR；判定线数值与 3.1.3 文档 §6 逐字一致）：

```text
INFO ash_renderer::host: 宿主壳启动:渲染族 8 插件已禁用,无 RenderApp / 无 wgpu 初始化
INFO ash_renderer::scene::material_hook: 材质缝自检:Assets<StandardMaterial> 已注册;GltfExtensionHandlers 挂载 1 个 handler(期望 1 = AshMaterialHook)
INFO ash_renderer::scene::lights: 灯光进场:方向光 20000 lx(FULL_DAYLIGHT)沿实体 forward;环境光用 GlobalAmbientLight 默认(LightPlugin 预插,不 spawn 实体)
INFO ash_renderer::scene::camera: 相机进场:Camera+Projection+Transform @ (0.70, 0.70, 1.00) 看向 (0.00, 0.30, 0.00)(官方示例取景,宽高比按主窗口)
INFO ash_renderer::scene::world_asset: 场景进场:已请求 models/FlightHelmet/FlightHelmet.gltf#Scene0,实体树待依赖就绪后异步展开
INFO ash_renderer::vulkan::context: Vulkan 进程级上下文就绪: API v1.3.289  设备 NVIDIA GeForce RTX 2060 (DISCRETE_GPU)  队列族 0
INFO ash_renderer::vulkan::swapchain: swapchain 就绪: 1600x900,3 images,B8G8R8A8_UNORM
INFO ash_renderer::vulkan::frames: 帧资源就绪:2 组在飞(命令缓冲 + 双信号量 + fence)
INFO ash_renderer::host: Vulkan 全链就绪:Context + Swapchain + 2 帧在飞;清屏循环自下一 Update 起
INFO ash_renderer::scene::camera: 相机就位:1 台 @ (0.70, 0.70, 1.00) 前向 (-0.54, -0.31, -0.78) 与位置→注视点夹角 0.0000°(looking_at 语义),宽高比 1.7778
INFO ash_renderer::scene::lights: 灯光就位:方向光 1 盏沿前向 (0.40, -0.45, -0.79);环境光 GlobalAmbientLight 亮度 80(LightPlugin 预插默认,相机组件 AmbientLight 可覆盖)
INFO ash_renderer::scene::world_asset: 场景进场到货:primitive 实体 6(期望 6),带 MeshMaterial3d 6/6(期望 6/6);容器——Mesh 6(期望 6)、StandardMaterial 6(期望 6 = 每材质一份,兜底 DefaultMaterial 无人持柄不驻留)、Image 17(期望 17 = 内置 2 + 文件 15)
```
