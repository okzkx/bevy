# DefaultPlugins 分类清单（第一步 Q2）

> 2026-09-14。回答四问之 Q2："RenderPlugin 禁掉后，DefaultPlugins 还剩什么在跑？"
> 分类标准 = 本项目边界：**留窗口、禁渲染、UI/音频/拾取非目标**。完整清单见 `crates/bevy_internal/src/default_plugins.rs`（34 条）。

## 0. 先说结论

1. **渲染族只需禁 8 个**（默认 feature 下实际在跑的），全是渲染族 crate 出品；
2. **意外的好消息：0.19.1 已把数据与渲染 crate 彻底解耦**——逐一核实 Cargo.toml，`bevy_image`/`bevy_mesh`/`bevy_camera`/`bevy_light`/`bevy_text`/`bevy_ui`/`bevy_scene`/`bevy_animation`/`bevy_gltf`/`bevy_sprite`/`bevy_state`/`bevy_window` **全部 0 依赖 bevy_render**。所以 Mesh/Camera/Light/Image 这些"看起来像渲染"的插件其实全是宿主侧数据插件，照常保留；
3. 唯一的琥珀项 `PbrPlugin` 已核实**优雅降级**：build 里全是 `if let Some(render_app) = app.get_sub_app_mut(RenderApp)` 模式（`bevy_pbr/src/lib.rs:301/332/357`），缺 RenderApp 时渲染部分静默跳过，而 `Assets<StandardMaterial>` 的注册发生在主世界——**保留它**，正好是取数表需要的材质容器。**〔0.20-dev 勘误 2026-09-21：此结论错——build 里还有无条件 `load_shader_library!`（0.19.1 未验证的最后一点），实测必炸，PbrPlugin 已移入禁用名单，详见 步骤 2《2-宿主壳搭建记录》§2〕**

## 1. 渲染族（禁用名单，8 个）

| 插件 | 来源 | 作用 | 备注 |
|---|---|---|---|
| `RenderPlugin` | bevy_render | 建 RenderApp + 初始化 wgpu | **主目标** |
| `PipelinedRenderingPlugin` | bevy_render::pipelined_rendering | 渲染线程流水线 | multi_threaded feature |
| `CorePipelinePlugin` | bevy_core_pipeline | 2D/3D 管线图 | |
| `PostProcessPlugin` | bevy_post_process | 后处理 | |
| `AntiAliasPlugin` | bevy_anti_alias | 抗锯齿 | |
| `SpriteRenderPlugin` | bevy_sprite_render | 2D 渲染 | bevy_sprite（数据）本身可留 |
| `UiRenderPlugin` | bevy_ui_render | UI 渲染 | bevy_ui（布局）本身无渲染耦合 |
| `GizmoRenderPlugin` | bevy_gizmos_render | 线框渲染 | |

默认 feature 下不装的渲染族（不用管）：`DlssInitPlugin`（dlss feature）、`RenderDebugOverlayPlugin`（bevy_dev_tools）。

## 2. 宿主族（保留）

| 组 | 插件 | 一句话 |
|---|---|---|
| 基础设施 | `PanicHandlerPlugin`、`TaskPoolPlugin`、`LogPlugin`、`FrameCountPlugin`、`TimePlugin`、`DiagnosticsPlugin`、`TerminalCtrlCHandlerPlugin` | panic hook / 线程池（渲染上传也能用）/ 日志 / 帧计数 / Time / 诊断 / Ctrl+C |
| 窗口 | `WindowPlugin`、`WinitPlugin`、`AccessibilityPlugin` | 窗口 + 事件循环（本项目留窗口的根基） |
| 输入 | `InputPlugin`、`InputFocusPlugin`、`InputDispatchPlugin` | 键鼠状态 / 焦点 |
| 资产 | `AssetPlugin`、`GltfPlugin`、`WebAssetPlugin`* | 资产核心 / glTF 加载（取数源头）/* 仅 http(s) feature |
| 场景 | `ScenePlugin`、`WorldSerializationPlugin` | BSN/DynamicScene |
| 数据组件 | `TransformPlugin`、`MeshPlugin`、`CameraPlugin`、`LightPlugin`、`ImagePlugin` | 变换传播（渲染数据核心）/ 网格 / 相机 / 灯光 / 贴图——全是 0 渲染依赖的数据插件 |
| 材质 | **`PbrPlugin`** | 琥珀→保留：注册 `Assets<StandardMaterial>`（主世界），渲染部分优雅跳过 |
| 动画/状态 | `AnimationPlugin`、`StatesPlugin` | CPU 蒙皮 / 状态机 |
| 条件性 | `ScheduleRunnerPlugin` | custom cfg = **无 bevy_window 时才装**——我们有窗口，它自动缺席 |

## 3. 无关族（非目标；建议禁用省开销，留着也基本无害——均无渲染耦合）

`AudioPlugin`（音频）、`GilrsPlugin`（手柄）、`TextPlugin`（文本光栅化，UI 供料）、`UiPlugin`（2D 布局）、`UiWidgetsPlugins`（嵌套插件组）、`DefaultPickingPlugins`（嵌套插件组）、`SpritePlugin`（2D 数据）、`GizmoPlugin`（线框存储）、`ClipboardPlugin`。

## 4. 宏彩蛋（读清单时顺带核实）

- 清单最后一条是 `:IgnoreAmbiguitiesPlugin`——**零路径段**写法（宏的 `$($plugin_path:ident::)*` 允许零次重复），`#[doc(hidden)]`，负责歧义抑制，最后装；
- `#[plugin_group]` 标记的条目（UiWidgetsPlugins/DefaultPickingPlugins）走宏的 `add_group` 分支——**组里套组**。

## 5. M1 验证清单（遗留到第二步执行）

1. 把上面 8 个渲染族插件写进宿主壳的 `disable` 名单；
2. 重点观察保留 `PbrPlugin` 后是否安静（`load_shader_library!` 在无 RenderApp 时的行为是最后未验证点）；
3. 若还有插件 panic，按报错逐个追加 disable（`disable` 是 TypeId 级，与顺序无关）。

## 6. 状态

Q2 ✅ 回答完毕（2026-09-14）。第一步剩 Q4（glTF→Mesh3d 链路）+ System 参数全家桶。
