# 第二步：宿主壳——禁渲染 Bevy + ash 清屏循环（M1）（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 第二步，本文件夹存放该步的全部产出。

**目的**：验证本项目根基——`DefaultPlugins.build().disable::<RenderPlugin>()` 与 winit 窗口、自建 Vulkan 能否共存。
**完成标准**：清屏窗口稳定运行、resize 不崩、退出干净；全程无 wgpu 初始化（RenderDoc / 调试日志确认）。

## 施工顺序（对应路线图"做什么"）

1. ✅ **定渲染器 crate 落位**（2026-09-21 已决：workspace member，crate 名 `ash_renderer`，位于仓库根，决策依据与验证见《宿主壳搭建记录》§1）；
2. 建 crate：组装 App，禁渲染族插件（名单 = step1《DefaultPlugins分类.md》禁 8 件，报错实测后增补连带禁项）；
3. 窗口句柄链（侦察结论）：`PrimaryWindow` 实体 → `RawHandleWrapper` 组件 → ash Instance/Device/Surface/Swapchain，全程主线程；
4. 帧循环：acquire → 清屏 → present，resize 重建 swapchain，`AppExit` 时反序拆除。

## 材料清单

- 《[窗口链路侦察：WinitPlugin、RawHandleWrapper与事件进ECS.md](窗口链路侦察：WinitPlugin、RawHandleWrapper与事件进ECS.md)》——开篇侦察（2026-09-21）：窗口创建是事件驱动（首窗在 runner `resumed`，非 PreStartup）、**`WinitWindows` 0.19.1 已改 thread_local 的路线图修正**、`RawHandleWrapper` 两条 surface 路径（ash-window trait / 手写 vkCreateWin32SurfaceKHR）、runner 帧心跳结构（about_to_wait → app.update()）
- 《宿主壳搭建记录.md》——主产出：已开档，落位决策见 §1（2026-09-21），坑与解法随施工累积

## 待决问题

- [x] crate 落位：**已决（2026-09-21）workspace member**，crate 名 `ash_renderer`，决策依据见《宿主壳搭建记录》§1
- [ ] surface 方案：ash-window crate vs 手写 Win32 surface（侦察篇 §3 有两条路径对比）
- [ ] 禁插件后的连带报错清单（实测，对照 step1 分类表补名单）
- [ ] 退出顺序设计：Vulkan 对象拆除 vs bevy runner 清场的时序（侦察篇 §5 风险备忘）
