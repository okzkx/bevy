# 步骤 2：宿主壳——禁渲染 Bevy + ash 清屏循环（M1）（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 步骤 2，本文件夹存放该步的全部产出。主产出（`2-宿主壳搭建记录.md`）与 README 留根，其余专题文档在 [材料/](材料/)。（编号约定（2026-09-22）：本步四项施工①~④ = **2.1~2.4**，见[路线图·编号约定](../学习目标实现步骤.md)；下文保留历史编号原文）

**目的**：验证本项目根基——`DefaultPlugins.build().disable::<RenderPlugin>()` 与 winit 窗口、自建 Vulkan 能否共存。
**完成标准**：清屏窗口稳定运行、resize 不崩、退出干净；全程无 wgpu 初始化（RenderDoc / 调试日志确认）。

**状态：✅ 收官（2026-09-21）**——四项施工全部完成并通过实测（清屏窗口颜色呼吸、最大化/还原重建两次、WM_CLOSE 干净退出，证据见《2-宿主壳搭建记录》§4）。

## 施工顺序（对应路线图"做什么"）

1. ✅ **定渲染器 crate 落位**（2026-09-21 已决：workspace member，crate 名 `ash_renderer`，位于仓库根，决策依据与验证见《2-宿主壳搭建记录》§1）；
2. ✅ **建 crate：组装 App，禁渲染族插件**（2026-09-21 完成：disable 名单 = 8 件渲染族 + 连带禁项 PbrPlugin + 手动 `CompressedImageFormatSupport(NONE)`；0.20.0-dev 实测启动无 WARN、无 wgpu，见《2-宿主壳搭建记录》§2）；
3. ✅ **窗口句柄链**（2026-09-21 完成：surface 方案已决 = 手写 Win32；`VulkanContext` 全链 Entry→Instance→Surface→Device→Swapchain 实测建链成功（RTX 2060，1600×900×3），见《2-宿主壳搭建记录》§3）；
4. ✅ **帧循环**（2026-09-21 完成：`draw_frame` = acquire→动态渲染清屏→present，两帧在飞；resize 消息驱动 `Swapchain::rebuild`（最大化/还原实测两次）；退出走官方 `OnAppExitSystems` 钩子按帧级→resize级→进程级反序拆除，WM_CLOSE 实测干净退出。首个真 Vulkan bug `ERROR_NATIVE_WINDOW_IN_USE_KHR`（先建后拆）当场抓获并修复。见《2-宿主壳搭建记录》§4）。

## 材料清单

- 《[窗口链路侦察：WinitPlugin、RawHandleWrapper与事件进ECS.md](材料/窗口链路侦察：WinitPlugin、RawHandleWrapper与事件进ECS.md)》——开篇侦察（2026-09-21）：窗口创建是事件驱动（首窗在 runner `resumed`，非 PreStartup）、**`WinitWindows` 0.19.1 已改 thread_local 的路线图修正**、`RawHandleWrapper` 两条 surface 路径（ash-window trait / 手写 vkCreateWin32SurfaceKHR）、runner 帧心跳结构（about_to_wait → app.update()）
- 《[VulkanContext字段释义：从Entry到Swapchain.md](材料/VulkanContext字段释义：从Entry到Swapchain.md)》——施工③配套（2026-09-21）：每个字段的"是什么/为什么拆这层/Unity-D3D 映射"，创建链依赖图 + 生命周期四层表（施工④的模块拆分依据；标题为历史名，类型现名 `vulkan::Context`）
- 《[Context与Swapchain分家：按生命周期拆分.md](材料/Context与Swapchain分家：按生命周期拆分.md)》——施工④配套（2026-09-21）：为什么按寿命切对象（一统结构的三笔账）、三兄弟分家清单、`rebuild` 先拆后建的求值顺序教训、帧级与 swapchain 解耦原理、Unity-D3D 映射与命名定案
- 《[错误处理语法糖：frenderer syntax与ash_renderer移植.md](../笔记/错误处理语法糖：frenderer syntax与ash_renderer移植.md)》——收官后增补（2026-09-21）：frenderer 自写错误处理糖全图谱 + `ash_renderer/src/syntax.rs` 移植记录（控制流族 8 宏 + LogDebug/WarnOrDefault + 新增第 9 件 `unwrap_or_panic!`（家族 panic 位，现无调用点））——**工具篇**
- 《[错误处理体系：两Tier思想与优雅退出.md](../笔记/错误处理体系：两Tier思想与优雅退出.md)》——收官后增补（2026-09-21）：**宪法篇**——用户两 Tier 错误处理思想（非必要不 panic：①不影响运行 warn 丢弃继续 ②影响运行冒泡 main 优雅退出）、VulkanError 类型层、try_init `?` 串链 + AppExit 优雅退出全链、`?` 可用=失败处理集中、实测证据与后续纪律；init 全程零 panic
- 《[帧流程：一帧之内各Vulkan对象如何接力.md](材料/帧流程：一帧之内各Vulkan对象如何接力.md)》——收官后增补（2026-09-22 同日两轮实测推翻重写）：**原理篇**——两条时间线读帧循环（CPU 五调用全异步、唯一真等在 fence 重录闸门），核心原则 **CPU 对信号量只配置、不读取、无法感知状态**；三同步对象"谁置位 / 谁等令"总表（一收一发，执行者无一为 CPU）、画布旅程闭环、两帧在飞时间线与 OUT_OF_DATE 控制流分流；配两张技术说明图（`_assets/`：双时间线图、画布旅程环图，旧图已删）
- 《[三者关系：Context、Swapchain与FramePool.md](材料/三者关系：Context、Swapchain与FramePool.md)》——关系总览（2026-09-22）：回答"三者是平行吗"——Bevy Resource 层平级、Vulkan 对象图一根两枝、寿命三层楼；各自代表什么（地基/画布租约/节拍器）、一帧内五步分工归属、两个独立循环（帧槽位 vs image index）、Unity-D3D 关系对照；配两张 AI 技术图（`_assets/`：一根两枝结构图、两个独立循环图）
- 《[PipelineStage：屏障与信号量的共同语言.md](材料/PipelineStage：屏障与信号量的共同语言.md)》——同步语义篇（2026-09-22）：下沉到闸门挂点——**stage 管时间、access 管内存**两把钥匙；屏障（流内：阶段×访问精配）与信号量（批间：整批对整批、只留"从哪个阶段卡住"一个旋钮）是同一语法两档粒度，本项目两道布局屏障逐参数拆解、`wait_dst_stage_mask=COLOR_ATTACHMENT_OUTPUT` 的精确含义；配一张闸门接力图（`_assets/stage-gates-frame-chain.png`）
- 《[写文档纪律：帧流程三轮返工的教训.md](../笔记/写文档纪律：帧流程三轮返工的教训.md)》——方法论篇（2026-09-22）：帧流程篇同日三轮返工的沉淀——**先机制后修辞、判定线立 §0、对着读者的下一个问题写、精简=删重复不删信息**；配图三层验证（OCR / 版式 / 逐箭头审语义）与锚点钉死规则
- 《[2-宿主壳搭建记录.md](2-宿主壳搭建记录.md)》——主产出（2026-09-21 **收官重写为终态总结**：一句话总结/终态架构（模块分层+宿主桥编排+关键决策表）/四项施工精要/合并坑表/证据汇总/步骤 3 接口；逐施工原始记录在 git 历史。**2026-09-22 步骤 3 开工结构整理**：三系统自 main.rs 迁入 `host.rs` 宿主桥，main 只做插件组装——编排内容不变，见搭建记录 §1）

## 待决问题

- [x] crate 落位：**已决（2026-09-21）workspace member**，crate 名 `ash_renderer`，决策依据见《2-宿主壳搭建记录》§1
- [x] surface 方案：**已决（2026-09-21）手写 Win32 surface**（零新依赖 + 练 Vulkan + 仅 Windows；ash-window 弃），依据见《2-宿主壳搭建记录》§3
- [x] 禁插件后的连带报错清单：**已实测（2026-09-21）**——`PbrPlugin` 必炸（`Assets<Shader>` 注册在 RenderPlugin 里）→ 连带禁；`CompressedImageFormatSupport` 需手动初始化；详见《2-宿主壳搭建记录》§2
- [x] 退出顺序设计：**已决并实测（2026-09-21）**——`teardown_vulkan` 挂 `Last → OnAppExitSystems`（bevy_time 同款官方钩子），在 `despawn_windows` 销毁 hwnd 前按帧级→resize级→进程级反序 `remove_resource`；不能等 `exiting` 回调的 `clear_all()`（顺序任意且窗口已死）。详见《2-宿主壳搭建记录》§4
