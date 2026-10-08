# 代码结构整理：main 只做统筹（宿主桥分家）

> 2026-09-22，步骤 3 开工整理（3.1.1 之后、3.2 之前）。动机：3.1.1 落地后 main.rs 已 230 行——init 链、75 行帧循环编排、teardown、清屏色全住在 main；3.2 上传、3.3 描述符、3.4 管线还要往帧循环里长，继续施工只会越来越乱。
> 用户定案：**每个模块做自己的事，main 理论上不执行任何底层调用，只调用各模块暴露的 API。**

## §0 判定线

| 项 | 状态 |
|---|---|
| main 零 Vulkan 符号（不 import vulkan/swapchain/frames/error/syntax，只见两个 Plugin 类型） | ✅ |
| `draw_frame` 编排与函数名原样保留（施工计划 §2 接口承诺不破） | ✅ |
| 行为零回退：clippy `-D warnings` 全干净 + 实跑启动链/清屏循环/WM_CLOSE 优雅退出全过 | ✅ |

## §1 终态：四层职责

| 层 | 文件 | 职责 | main 能看见什么 |
|---|---|---|---|
| 统筹 | `main.rs`（~50 行） | 禁渲染名单（含 PbrPlugin 连带禁注释）+ `add_plugins` + `run()`；`-> AppExit` 退出码契约 | 只见 `AshMaterialHookPlugin` / `AshHostPlugin` 两个类型名 |
| 宿主桥（编排） | `host.rs` `AshHostPlugin` | 三系统进调度（`Startup` init_vulkan / `Update` draw_frame＋resource_exists 守卫 / `Last` teardown_vulkan×OnAppExitSystems）+ 禁渲染补位（`CompressedImageFormatSupport` 自报）+ announce | — |
| 资源（生命周期） | `vulkan` / `swapchain` / `frames` | 进程级 / resize 级 / 帧级，**本次零改动** | — |
| 数据（ECS 取数） | `scene` | 材质缝接线与场景采集，零 Vulkan；插件自含 | — |

host.rs 特意一个文件装三系统：资源生命周期是一根完整的线（插入顺序 = 创建顺序 → 退出反序拆除），拆成 init/loop/teardown 三个文件会把这根线剪断，读的时候要跳三处。编排层不持有 Vulkan 状态，只编排三个资源模块暴露的类型——录制细节在 frames.rs、重建细节在 swapchain.rs，host 只做接线和错误分流。

## §2 迁移清单

| 动作 | 内容 |
|---|---|
| 新增 `host.rs` | `init_vulkan` / `try_init_vulkan` / `draw_frame` / `clear_color` / `teardown_vulkan` / `announce` 自 main.rs **逐字迁入**（函数保持私有，模块只出 `AshHostPlugin`）；两 Tier 失败策略的模块级注释随迁 |
| 随迁 | `CompressedImageFormatSupport(NONE)` 自报从 main 的 `insert_resource` 移入 `AshHostPlugin::build`——它是"禁渲染后须有人补位"的运行期职责，归宿主桥 |
| 改 `scene.rs` | `report_material_seam` 注册权移交 `AshMaterialHookPlugin::build`（模块完全自含，main 不再引用 scene 内部件） |
| 改 `lib.rs` | 挂 `pub mod host`，模块分层清单补宿主桥一行 |
| 删 `main.rs` | 上述全部；禁用名单及其注释**留在 main**——装什么禁什么是统筹决策，不是底层代码 |

## §3 给 3.2~3.5 的边界约定（本次整理的目的）

1. **新模块照 scene 的样子落位**：3.2 `resources.rs`（内存类型 helper + 顶点/索引池）、3.3 描述符——各自暴露 Plugin（或 init 函数）+ 资源类型，注册进调度靠插件，main 不新增 import；
2. **帧循环生长只动两处**：host.rs 的 `draw_frame` 编排（加步骤）与 frames.rs 的录制段（`record_clear_and_submit` → `record_frame`）——施工计划 §2 与搭建记录 §6 的既定承诺；
3. **main 从此冻结**：只允许"增删一个 `add_plugins`"级别的改动；出现第三行业务逻辑即违规，回炉进模块。

## §4 验证证据（2026-09-22）

- `cargo clippy -p ash_renderer --all-targets -- -D warnings`：通过（仅顺手修 1 处新 doc 注释的 doc_lazy_continuation）；
- 实跑启动日志（目标已是新模块路径）：

```text
INFO ash_renderer::host: 宿主壳启动:渲染族 8 插件已禁用,无 RenderApp / 无 wgpu 初始化
INFO ash_renderer::scene: 材质缝自检:Assets<StandardMaterial> 已注册;GltfExtensionHandlers 挂载 1 个 handler
INFO ash_renderer::vulkan: Vulkan 进程级上下文就绪: API v1.3.289  设备 NVIDIA GeForce RTX 2060 (DISCRETE_GPU) 队列族 0
INFO ash_renderer::swapchain: swapchain 就绪: 1600x900,3 images,B8G8R8A8_UNORM
INFO ash_renderer::frames: 帧资源就绪:2 组在飞(命令缓冲 + 双信号量 + fence)
INFO ash_renderer::host: Vulkan 全链就绪:Context + Swapchain + 2 帧在飞;清屏循环自下一 Update 起
```

- WM_CLOSE 优雅退出链复测：`bevy_window: No windows are open, exiting` → `ash_renderer::host: 退出拆除完成:Vulkan 资源已按帧级→resize级→进程级反序移除` → `Closing window` → 进程退出（残留进程已清理）。
