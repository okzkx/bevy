# 窗口闸门：最小化死锁与 resize 卡顿的实测与修复

> 2026-09-22，3.1 段收官后的窗口缺陷专项。两个用户可见 bug、一次设计返工、一轮调试方法论教训。
> 改动落点：`ash_renderer/src/host.rs`（`draw_frame` 的 `ResizeGate` + `WinitSettings` 节流）+
> `ash_renderer/src/vulkan/swapchain.rs`（**MAILBOX 优先选型**）+
> `crates/bevy_winit/src/state.rs`（**fork 内修**：`Resized(0,0)` 不写 Window 组件）。
> 定案修订（同日）：初版"拖拽中整帧让路"被用户否决——**宁要渲染卡顿，不要冻结**；
> 翻出 frenderer 当年的解法（MAILBOX + dirty 下帧重建 + CPU 侧帧率门）照方抓药。

## §0 判定线（修复后全部实测通过）

| 项 | 判定 | 实测 |
|---|---|---|
| 最小化保持 | SC_MINIMIZE 后窗口保持最小化、不冻结（`hung=False`） | ✅ 2s+ 稳定 |
| 任务栏还原 | WM_SYSCOMMAND(SC_RESTORE) 还原到**原始全尺寸**，闸门重开恢复渲染 | ✅ 应用日志`窗口脱离最小化：闸门重开` |
| 拖拽持续 | 25 步程序化拖拽（每步 +16px/16ms）→ **帧持续流动零冻结**、每步恰好一次重建、零挂死 | ✅ 26=启动1+25步，循环墙钟 1.16s |
| 渲染节奏 | MAILBOX 去背压后 update 以官方 `WinitSettings` Reactive(1/60) 节流 | ✅ 空闲 CPU 214%→37% |
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

## §2 设计两轮返工：冻结被否决，frender 解法胜出

**第一版（停歇去抖）**：最小化整帧让路 + 拖拽中去抖、停歇后重建，但拖拽中**继续 present 旧尺寸帧**。实测拖拽第 3 步即挂死（`hung=True`）——**对失配 swapchain 的反复 acquire/present 同样阻塞驱动**（FIFO 下）。旧代码"消息一到就先重建"恰好歪打正着地避开了它。

**第二版（统一闸门，整帧让路）**：失配期间不碰 Vulkan，画面由 DWM 持有的最后一帧顶住。判定线全过——但**用户拍板否决冻结**："与其冻结，不如渲染卡顿，目标应当是减少卡顿"，并指路 frenderer 当年解决过同一问题。

**第三版（终案，frenderer 三件套移植）：**

| frenderer 先例 | ash_renderer 落地 | 效果 |
|---|---|---|
| `PresentMode::MAILBOX`（FIFO 兜底，swapchain_context.rs:78） | 同款选型 + MAILBOX 时 image_count 提到 ≥3 | present 替换不排队、acquire 永不阻塞 → 拖拽中帧循环不断流，FIFO 的 acquire 阻塞（死锁根源）整体消失 |
| `dirty_swapchain` 标记 → 下一帧 `MainEventsCleared` 时 drop+重建（render_window.rs:174） | `ResizeGate.pending` → 帧首按需重建（幂等），acquire 永远落在新 swapchain 上 | 每步一建、内容持续跟上 |
| `try_draw_new_frame` 帧率门（1/刷新率，render_window.rs:82） | 官方旋钮 `WinitSettings::Reactive(1/60)`（focused/unfocused 同 60Hz） | MAILBOX 无背压不再烧 CPU：空闲 214%→37% |

- 最小化闸门保留：失配特例，整帧让路保消息泵（§1 死锁的唯一直接解）；
- acquire 两道防线不变：SUBOPTIMAL 画完标 pending、OUT_OF_DATE 立即重建；
- `device_wait_idle` 保留（严格规范安全）；MAILBOX 下在途 present ≤1，排干代价更小；
- 已知取舍：拖拽中每步重建 ≈28~60ms 的顿挫仍在（MAILBOX 让它不再叠加 acquire 阻塞），按用户定案"接受卡顿、消灭冻结"；后续若要再压，方向是 wait_idle→frame-fence 化（3.4 后再议）。

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

- `host.rs`：`ResizeGate`（`Local`，minimized/pending/logged_minimized）+ `draw_frame` 三道闸门；`AshHostPlugin` 插入 `WinitSettings::Reactive(1/60)` 节流（官方机制，节住整个 update 循环）。
- `bevy_winit/src/state.rs`：`Resized(0,0)` 过滤（§3）——**fork 分歧点，同步上游时需重放**。
- `swapchain.rs`：`rebuild` 文档补成本契约（≈60ms/次，调用方必须去抖）。
- Unity/D3D 对照：D3D 交换链 `ResizeBuffers` 无需销毁重建整条链、且 DXGI 有 `DXGI_STATUS_OCCLUDED` 可查询；Vulkan WSI 的"失配即阻塞"只能靠应用层闸门——本篇的闸门即 Vulkan 世界的 `OCCLUDED` 处理位。
