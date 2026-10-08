# 窗口链路侦察：WinitPlugin、RawHandleWrapper 与事件进 ECS

> 2026-09-21，步骤 2 开篇侦察（原 步骤 1 做什么第 6 项"窗口链路定点侦察"，推迟到本步做）。源码：`crates/bevy_winit/src/{lib,state,system,winit_windows}.rs`、`crates/bevy_window/src/raw_handle.rs`。

## 0. 一句话结论

**宿主壳拿 surface 不需要碰 `WinitWindows` 内部**：spawn 窗口实体后，bevy_winit 会自动把 **`RawHandleWrapper` 组件**插到窗口实体上（内含 `RawWindowHandle`/`RawDisplayHandle` 副本 + 保活窗口的 Arc），Startup 系统里 `Query<&RawHandleWrapper>` 直达 ash surface。

## 1. 窗口从哪来：事件驱动的创建链（不是 PreStartup system）

`WinitPlugin::build`（lib.rs:90-158）四件事：

1. **加插件时就建好 `EventLoop<WinitUserEvent>`**（Windows 可 `run_on_any_thread`），同时插入两个资源：`DisplayHandleWrapper`（`OwnedDisplayHandle`，纯 display 侧集成用）和 `EventLoopProxyWrapper`（从外部唤醒 loop 用）；
2. **`set_runner(winit_runner)`**——App 的主循环从此归 winit 管；
3. 加 `Last` 尾部系统链：`changed_windows`（组件改动回写 winit）→ `changed_cursor_options` → `despawn_windows`（ExitSystems 后）→ `check_keyboard_focus_lost`；
4. **观察者 `On<Add, Window>`** → proxy 发 `WinitUserEvent::WindowAdded`。

建窗时序（关键：事件驱动，两处入口）：

- **首窗**：runner 的 `resumed` 回调直接调 `create_windows`（state.rs:173-182）——发生在任何 schedule 之前；
- **后继窗**：spawn 带 `Window` 组件的实体 → Add 观察者 → proxy → runner `user_event(WindowAdded)` → `create_windows`（state.rs:184-198）。

`create_windows`（system.rs:49）扫 `Added<Window>`，对每个新实体：

- 按 `Window`/`CursorOptions` 组件拼 winit attributes（注意 AccessKit 流程：**先隐形建窗**，adapter 就绪后 `set_visible`——state.rs 里 Windows 还有"全隐形窗口强制跑一轮 update"的补丁）；
- 插 `CachedWindow` + `WinitWindowPressedKeys`；
- **`RawHandleWrapper::new(winit_window)` 成功就作为组件插到窗口实体上**（system.rs:94-98）；实体若带 `RawHandleWrapperHolder` 则同步填一份；
- 写 `WindowCreated` message。

## 2. ⚠️ 路线图修正：WinitWindows 已不是 NonSend 资源

路线图第二步写的是"`WinitWindows`（NonSend）→ raw-window-handle"，**0.19.1 实情**：`WinitWindows` 存在 `thread_local WINIT_WINDOWS: RefCell<WinitWindows>`（lib.rs:53-57，注释自述是 #17667 完成前的临时方案），系统里用 `WINIT_WINDOWS.with_borrow(...)` 访问（pub），主线程亲和靠 `NonSendMarker`。它的本职是"实体 ↔ winit 窗口"双向映射和建窗参数拼装；我们只在**动态开关窗口**这类场景才需要直接碰它。

## 3. raw handle 获取：RawHandleWrapper（bevy_window/raw_handle.rs）

结构（raw_handle.rs:52-62）：

```rust
pub struct RawHandleWrapper {
    _window: Arc<dyn Any + Send + Sync>,  // 保活 winit window——渲染器在飞帧期间窗口不会被 drop
    window_handle: RawWindowHandle,       // Windows 上 = Win32 { hwnd, hinstance }
    display_handle: RawDisplayHandle,
}
```

- `unsafe impl Send + Sync`（raw_handle.rs:127-129）：指针合法性约定推给调用方（"only used in valid contexts"）——Windows 桌面端主线程用没问题；
- 两条消费路径：
  1. **`unsafe fn get_handle()`** → `ThreadLockedRawWindowHandleWrapper`，它 impl 了 `HasWindowHandle + HasDisplayHandle`（raw_handle.rs:140-162）——**ash-window `create_surface` 正好吃这个 trait**，unsafe 只在 get_handle 一处；
  2. 直接 `get_window_handle()` 拿 `RawWindowHandle::Win32{hwnd, hinstance}` 手写 `vkCreateWin32SurfaceKHR`（frenderer 同思路）；注意 hinstance 可能为 None，要 `GetModuleHandleW` 兜底；
- 退出语义的隐含约定：Android 挂起时框架主动移除窗口实体的 `RawHandleWrapper` 触发 surface 销毁（state.rs:540-550）——**"组件在 = surface 可用"是框架层面的生命周期信号**，桌面端我们的 surface 生命周期完全自管，必须自己保证"先 drop Device/Instance 再让 winit 销窗"。

## 4. 事件怎么进 ECS（state.rs，1233 行的 runner）

`winit_runner` 是 App 的 runner；`WinitAppRunnerState` impl `ApplicationHandler<WinitUserEvent>`，关键分工：

- **winit 回调里不进 ECS**：`window_event`（state.rs:200）只做三件事——按 `WindowId` 查实体（`WINIT_WINDOWS` 反查表）、把 winit 事件转成 bevy `WindowEvent` 塞进 runner 内的暂存队列（Resized 会先经 `react_to_resize` 改写 `Window` 组件）、把原始事件存 `RawWinitWindowEvent`（lib.rs:192，power-user 逃生门）；
- **`about_to_wait` → `redraw_requested`（state.rs:513）才是帧心跳**：按 `WinitSettings.update_mode(focused)` 判定（Continuous/Reactive），该跑就 `run_app_update()` = **`app.update()`**（state.rs:620），把暂存消息在帧开头写进 ECS message 队列；update 期间系统发的 `RequestRedraw`/`WindowCloseRequested` 会被读出来驱动下一轮（state.rs:630-650）；
- `exiting` 回调：清空 `WINIT_WINDOWS` + `world.clear_all()`（state.rs:502-509）——**退出顺序的硬边界：这时窗口要没了，Vulkan 对象必须已经先死**。

一帧时序：winit 事件回调（转换+暂存）→ about_to_wait → `app.update()`（PreUpdate 写消息 → Update → PostUpdate → Last 回写 winit）→ request_redraw → 下一个 loop 迭代。

**对宿主壳的含义：我们的"帧循环"不是自己写的 while，而是住在 bevy runner 里**——清屏 = Update 系统里 acquire/present；`UpdateMode::Continuous` 保证窗口可见时每帧被驱动；resize 用 `MessageReader<WindowResized>` 触发 swapchain 重建；退出监听 `AppExit`，在 Last 里抢在 runner 清场前 drop Vulkan 对象。

## 5. 宿主壳接线清单（ash 侧）

1. App 骨架：`DefaultPlugins.build().disable::<RenderPlugin>()`（渲染族名单 = 步骤 1《DefaultPlugins分类.md》禁 8 件，报错实测后增补）；
2. `commands.spawn((Window::default(), PrimaryWindow))`；
3. **Startup**（此时 `resumed` 已跑过，`RawHandleWrapper` 必在）：`Query<&RawHandleWrapper, With<PrimaryWindow>>` → ash entry → Instance → Surface（路径 1 或 2）→ Device → Swapchain——全程主线程；
4. Update：acquire → 清屏 → present；`MessageReader<WindowResized>` 触发重建；`AppExit` 时按 Instance ← Surface ← Device 反序拆除。

风险备忘：

- `hinstance` 可能缺：Win32 handle 的 hinstance 字段是 `Option`，ash surface 需要 instance 句柄，用 `GetModuleHandleW(None)` 兜底；
- 退出顺序是本项目第一个真正的坑位：bevy 退出流程（`exiting` 回调）与 Vulkan 对象销毁的交错，第一版先在 `AppExit` observer 里抢跑，崩了再改设计；
- resize 当帧 swapchain 可能返回 `ERROR_OUT_OF_DATE`，重试一帧即可，不必当错误。
