# 3.9 鼠标摄像机控制（追加段，2026-10-09）

> 收官后补段（3.6/3.7/3.8 同系列，先于步骤 4）。用户需求：①左键按住屏幕拖动，摄像机围绕中心点旋转；②滚轮让摄像机到中心点前后移动，产生缩放效果。

**目的**：给 3.7 立起来的调试 GUI 补上检视手段——固定中心点的环绕相机（检视相机），模型任意角度可看、远近可调。

**边界**：不做平移（pan）、不做 picking 命中定中心、不做动量/平滑；官方 `bevy_camera_controller::pan_orbit_camera`（0.21 新增）是完整版参照，其 `bevy_render` 不可选依赖在禁渲染宿主挂不上。零 Vulkan 代码，不碰帧循环。

## 任务

| 任务 | 范围 | 状态 |
|---|---|---|
| 3.9.1 | 左键拖拽环绕：屏幕位移 → 球角（yaw/pitch），`2π/窗口高` 归一，俯仰限位 ±89° | ✅ |
| 3.9.2 | 滚轮推拉：每格 ×0.95 指数缩放半径，限位 [0.5, 10.0] | ✅ |

## 判定线（收官总账，2026-10-09）

1. 注入拖拽 (+300,+150) 物理像素：头盔正视 → 俯视后脑勺，构图以头盔为中心 ✅
2. 滚轮只改距离不改角度：滚远变小/滚近变大，yaw/pitch 与初始严格一致（日志 0.611/0.317 rad）✅
3. egui 仲裁门：面板内拖拽归 egui（文本选择高亮）、相机不动；视口输入归轨道 ✅
4. clippy 全净（lib+tests）✅；零 VUID + WM_CLOSE 优雅退出 exit 0 ✅

## 产物

- 代码：`ash_renderer/src/scene/mechanism/camera_control.rs`（新，[`CameraOrbit`] + [`AshCameraControlPlugin`]）；`scene/content/camera.rs`（spawn 时挂轨道组件）；`scene/mechanism/mod.rs`、`scene/mod.rs`（出口）；`main.rs`（一行 add_plugins）。
- 工具：`tools/capture_window.py` / `tools/inject_mouse.py` / `tools/focus_window.py`（截图取证、输入注入、置前台，后续段的窗口回归直接复用）。
- 施工记录：[3.9.1-3.9.2-鼠标摄像机控制施工记录：左键环绕、滚轮推拉与egui层仲裁门](3.9.1-3.9.2-鼠标摄像机控制施工记录：左键环绕、滚轮推拉与egui层仲裁门.md)——含 egui 0.36 `is_pointer_over_egui()` 在 `begin_pass` 形状下恒真的源码级根因、PrintWindow 连拍亮度爬升伪影、winit 滚轮合并、窗口层叠漂移四枚钉子。

## 给步骤 4 的接口

- 交互件与采集/上传链路完全解耦：只读 bevy 输入资源 + egui 层序查询，写相机 `Transform`，PostUpdate 传播后 Last 同帧消费——步骤 4 的对象增量、驻留账本可视化检视直接沿用。
- `tools/` 三件套补齐了"注入输入 + 截图 + 置前台"的程序化回归通路，3.7 的窗口回归矩阵可升级为带交互的版本。
