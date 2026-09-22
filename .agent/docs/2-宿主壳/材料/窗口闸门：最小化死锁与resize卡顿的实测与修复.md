# 窗口闸门：最小化死锁与 resize 卡顿的实测与修复

> 2026-09-22，3.1 段收官后的窗口缺陷专项。两个用户可见 bug、一次设计返工、一轮调试方法论教训。
> 改动落点：`ash_renderer/src/host.rs`（`draw_frame` 的 `ResizeGate`，重写编排）+
> `crates/bevy_winit/src/state.rs`（**fork 内修**：`Resized(0,0)` 不写 Window 组件）+
> `ash_renderer/src/vulkan/swapchain.rs`（仅注释：rebuild 成本契约）。

## §0 判定线（修复后全部实测通过）

| 项 | 判定 | 实测 |
|---|---|---|
| 最小化保持 | SC_MINIMIZE 后窗口保持最小化、不冻结（`hung=False`） | ✅ 2s+ 稳定 |
| 任务栏还原 | WM_SYSCOMMAND(SC_RESTORE) 还原到**原始全尺寸**，闸门重开恢复渲染 | ✅ 应用日志`窗口脱离最小化：闸门重开` |
| 拖拽流畅 | 25 步程序化拖拽（每步 +16px/16ms）→ **零中途重建**、零挂死 | ✅ |
| 停歇重建 | 停歇 ≥150ms 后**恰好一次**重建到位 | ✅ `swapchain 就绪: 2100x901` |
| 渲染恢复 | 还原/重建后帧循环恢复满节奏 | ✅ CPU 增量 ≈1.1s/2s（vsync 节拍） |
| 优雅退出 | WM_CLOSE → 反序拆除 → 进程退出 | ✅ `退出拆除完成` |

## §1 两个 bug 与根因

### Bug 1：最小化后点任务栏无法还原——**stale acquire 死锁**

事件链（全部实测坐实）：

1. 最小化时 winit 发 `Resized(0,0)`（`winit-0.30.13` event_loop.rs 的 WM_SIZE 处理无条件转发）；
2. 旧版 `draw_frame` 对 resize 消息调 `rebuild` → `rebuild` 查 caps 发现 `current_extent=0`，**跳过重建但保留旧 swapchain**；
3. 帧循环继续走 → 对已 stale 的旧 swapchain `acquire` → **无限阻塞**（驱动对不可见 surface 的 FIFO 行为）；
4. 阻塞点在 runner 的 `app.update()` 里 → **winit 消息泵冻死** → 任务栏还原点击的 `WM_SYSCOMMAND(SC_RESTORE)` 永远轮不到处理。

取证：`IsHungAppWindow=True`（泵死 5s+）、SC_RESTORE 与 ShowWindow 双双无效、日志冻结在 `ACQUIRE-OUT-OF-DATE`。还原必须由应用自己泵消息才能完成——**死锁是自锁**：驱动等窗口可见，窗口等消息泵，消息泵等 acquire。

### Bug 2：拖拽 resize 的卡顿感——**每步一次 ~60ms 的同步重建**

程序化拖拽（25 步）实测每帧耗时分布：

| 相位 | 耗时 |
|---|---|
| `device_wait_idle` | **≈32ms（2 个 vsync：排干在途 present）** |
| `vkDestroySwapchainKHR` | ≈6ms |
| `vkCreateSwapchainKHR` | ≈22ms（驱动侧与 DWM 同步） |
| 每步合计 | **≈60ms+（拖拽全程 ~12fps，肉眼可见顿挫）** |

另测得拖拽期间 DWM 不提供 vsync 背压：acquire 不再阻塞，帧循环以 ECS 更新速度裸奔（3~5ms/帧），徒然灌满 present 队列。

## §2 设计返工：从"去抖重建"到"统一闸门"

第一版修复（**错误，已废弃**）：最小化整帧让路 + 拖拽中去抖、停歇后重建，但拖拽中**继续 present 旧尺寸帧**（DWM 拉伸顶住）。实测拖拽第 3 步即挂死（`hung=True`）——**尺寸失配不限于最小化：对失配 swapchain 的反复 acquire/present 同样阻塞驱动**。旧代码"消息一到就先重建"恰好歪打正着地避开了它。

定案（统一闸门，主流引擎"拖拽冻结画面、松手重排"同款）：

> **窗口尺寸与 swapchain 不一致期间（消息持续到达、未停歇）整帧让路，一次 Vulkan 都不碰；消息停歇 [`DRAG_QUIET=150ms`] 后重建一次，再恢复渲染。**

- 最小化 = 失配的特例（(0,0) 消息置 `minimized`），同一闸门覆盖；
- acquire 仍保留两道防线：SUBOPTIMAL → 画完这帧、标 pending（闸门随即拦住后续帧）；OUT_OF_DATE → 立即重建（那个状态连一帧都画不了）；
- `device_wait_idle` 保留（严格规范安全的经典模式）；重建降为停歇后的一次性动作，60ms 不可感知。

## §3 fork 内修：`Resized(0,0)` 不写 Window 组件

`crates/bevy_winit/src/state.rs`：最小化的 `(0,0)` 原本经 `react_to_resize` 写进 `Window` 组件，把分辨率污染成 0，还给 `changed_windows` 的尺寸回写（`request_inner_size(0x0)` → 对图标态窗口 SetWindowPos 强制还原）开了通道。改为：**非零尺寸照常写组件并发消息；零尺寸只广播 `WindowResized{0,0}` 消息、不写组件**——组件分辨率保持最后有效尺寸，窗口安分待在最小化。

（附调试教训：本条最初被误判为"上游 bug 在官方示例也复现"——实为探针缺陷，见 §5。）

## §4 探针与实测方法（调试方法论）

脚本化验证用 PowerShell + P/Invoke 驱动目标窗口，本专项踩了三颗钉子，后来者务必先读：

1. **进程拥有多个顶层窗口**：`EnumWindows` 下本进程至少有 5 个——bevy 主窗（class `Window Class`）、winit 事件窗口（14x14）、**conhost 控制台窗（class `ConsoleWindowClass`，默认 ~991x518）**、两个 IME 辅助窗。按"最大可见有标题窗口"选目标，**主窗最小化后（图标占位面积仅 ~4293）控制台必然胜出**——所有"窗口缩水成 991x518 / 自行还原"的观测都是它在冒充。
2. **P/Invoke 字符串必须 `CharSet = CharSet.Unicode`**：`GetWindowTextW/GetClassNameW` 的 StringBuilder 参数漏标时被截成 1 字符，类名过滤（跳过控制台）悄悄失效。
3. **测最小化一律用 `WM_SYSCOMMAND`（SC_MINIMIZE/SC_RESTORE）**：`ShowWindowAsync` 直接 ShowWindow 会绕过 winit 的 WM_SYSCOMMAND 标志同步，造成 winit 内部 `WindowFlags::MINIMIZED` 与 OS 脱钩，后续 flags 重应用时强制 SW_RESTORE——假性自恢复的又一来源。
4. 顺带：PowerShell 5.1 的 `Add-Type` C# 前端只认 C#5（匿名委托须显式委托类型）；脚本块转 .NET 委托时变量作用域断链（`listwin.ps1` 全程输出空）。

**修复后判定以应用侧日志为准**（闸门状态转移三日志：最小化让路 / 脱离重开 / 停歇按需重建），OS 侧状态只作旁证。

## §5 涉及文件与要点

- `host.rs`：`ResizeGate`（`Local`，minimized/pending/last_resize/logged_minimized）+ `draw_frame` 三道闸门；`DRAG_QUIET=150ms` 是调参点（调小→内容跟上更快但中途停顿更频繁）。
- `bevy_winit/src/state.rs`：`Resized(0,0)` 过滤（§3）——**fork 分歧点，同步上游时需重放**。
- `swapchain.rs`：`rebuild` 文档补成本契约（≈60ms/次，调用方必须去抖）。
- Unity/D3D 对照：D3D 交换链 `ResizeBuffers` 无需销毁重建整条链、且 DXGI 有 `DXGI_STATUS_OCCLUDED` 可查询；Vulkan WSI 的"失配即阻塞"只能靠应用层闸门——本篇的闸门即 Vulkan 世界的 `OCCLUDED` 处理位。
