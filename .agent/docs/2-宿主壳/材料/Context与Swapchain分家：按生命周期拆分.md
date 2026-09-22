# Context与Swapchain分家：按生命周期拆分

> 2026-09-21，施工④产物 + 命名定案（`VulkanContext` → `vulkan::Context`）。配套阅读：《VulkanContext字段释义：从Entry到Swapchain.md》（逐字段详解，文中类型名为历史名）、《宿主壳搭建记录.md》§4（施工过程与实测证据）。

## 0. 一句话结论

**分家不是"按功能切文件"，是按寿命切对象**：原来一个 `VulkanContext` 装了四种寿命的东西，谁该重建、谁先死、谁跨重建复用全靠注释声明；拆开后每个类型自带一个 `Drop`、一段寿命、一个重建动作，名字即答案。

## 1. 为什么分家：一统结构的三笔账

`VulkanContext`（施工③形态）在一个大 struct 里同时持有：

- **进程级**：Entry / Instance / 验证层 messenger
- **窗口级**：Surface
- **resize 级**：swapchain + images + format/extent
- **帧级**：命令缓冲、双信号量、fence（施工④才引入，但初始设计里本会塞进同一个结构）

一统结构的代价在施工④被逐笔兑现：

1. **重建动作非法**。resize 要"只换 swapchain 部分"，但对一个大 struct 没有这个 API 动作——要么重建全体（荒谬：Instance/Device 何辜），要么手动管理子集的生命周期，两种都别扭；
2. **Drop 责任表达不了**。`world.clear_all()` 对 Resource 的 drop 顺序是任意的，而"swapchain 必须死于 Device 之前"是跨资源的硬约束——一统结构里这约束只存在于注释里；拆开后它变成"谁的 Drop 写什么销毁序列"的代码事实；
3. **错误语义撑不住**。`Result<_, String>` 表达不了"`ERROR_OUT_OF_DATE_KHR` 不是失败，是控制流分支（重建后重试）"——帧循环的核心逻辑就是要 match 它。

## 2. 分家清单：三兄弟 + 错误

| 类型 | 模块 | 寿命 | 持有 | 重建动作 | Drop 销毁序 |
|---|---|---|---|---|---|
| `vulkan::Context` | `vulkan.rs` | 进程级 | Entry / Instance / messenger / **Surface** / PhysicalDevice / Device / Queue | 不重建 | Surface → Messenger → Device → Instance（wait_idle 先行） |
| `swapchain::Swapchain` | `swapchain.rs` | resize 级 | swapchain + images + **views** + format / extent | `rebuild()`：重查 caps → wait_idle → **先拆旧后建新** | views → swapchain（幂等，句柄清 null） |
| `frames::FramePool` | `frames.rs` | 帧级 | 命令池 + 2 组（命令缓冲 / image_available / render_finished / in_flight fence） | 不重建，**跨 swapchain 重建轮转复用** | 同步对象逐个 → 命令池（连带释放命令缓冲） |
| `error::VulkanError` | `error.rs` | — | `SwapchainOutOfDate` / `Vk(vk::Result)` / `Init(String)` | — | — |

两张表各管一件事的判定线：**swapchain 的 image 换一轮，FramePool 一根毫毛不动**——这就是"帧级不属于 resize 级"的物理体现。

## 3. 为什么 rebuild 必须"先拆后建"（首个真 bug 的教训）

`rebuild` 初版写的 `*self = Self::new(ctx)?` 看似优雅——"RHS 建新的，赋值时旧值触发 Drop"。**Rust 语义上这确实是先求值 RHS 再 drop 旧值**，但 Win32 WSI 限制同一 hwnd 同时只能挂一个 swapchain，于是"先建后拆"当场 `ERROR_NATIVE_WINDOW_IN_USE_KHR`，且因重建失败后旧 swapchain 还活着，每帧重试每帧失败。

两条修正，都是显式化：

```rust
unsafe { ctx.device.device_wait_idle() }?;  // 在飞帧还引用旧 image，先等完
self.destroy();                              // 幂等销毁（views → swapchain，句柄清 null）
*self = Self::new(ctx)?;                     // 再建新的；失败时 self 是空壳，下帧 resize 消息重试
```

- **显式 `destroy()` 代替"借赋值顺带 Drop"**：销毁时序（views 先于 swapchain）是代码事实，不是求值顺序的副作用；
- **幂等兜底**：句柄清 null 后重复 destroy 是空操作，`Drop` 复用同一方法——"取 images 失败就地销毁"的洞也一并堵上（否则坏 swapchain 一直占着 hwnd，重试永远 IN_USE）。

经验法则：**"旧值 Drop 时机"是求值顺序的产物，凡是销毁有时序要求的，把销毁写成显式方法**。

## 4. 帧级为什么不属于任何一张 swapchain image

FramePool 的 2 组资源按**帧槽位**轮转，不按 image 索引：

- `image_available` / `render_finished` 信号量桥接的是"present engine ↔ GPU ↔ present engine"，不认识具体哪张 image；
- `in_flight` fence 是 CPU 闸门：同组资源两次使用至少隔 `MAX_FRAMES_IN_FLIGHT=2` 帧，fence 保证上一轮提交已全部执行完，重录才安全——这是"每 image 一份同步对象"之外的合法简化，前提恰恰是**轮转与 swapchain 重建完全解耦**；
- 因此 `frame 游标`（`current`，MOD 2）与 `acquire 返回的 image index`（MOD image 数，FIFO 下 3）是两个独立循环，绝不互相推导。

resize 重建 swapchain 时，帧资源原地不动、命令池不动——分家之前这靠"都在一个大 struct 里所以一起活着"的巧合，分家之后这是结构本身。

## 5. Unity / D3D 映射

| 本项目 | D3D / Unity 对应物 | 分家视角 |
|---|---|---|
| `vulkan::Context` | `IDXGIFactory` + `ID3D11Device`（+HWND 绑定）的持久集合 ≈ Unity `GfxDevice` | 设备/工厂/交换链目标，进程活它活 |
| `swapchain::Swapchain` | `IDXGISwapChain`（backbuffer 轮换租约） | D3D 里 ResizeBuffers 换的是租约不是设备——Vulkan 里手动拆旧建新，同一语义 |
| `frames::FramePool` | D3D12 的 frame fence + command allocator 每帧轮转 | "帧槽位"概念 D3D12 官方化（FrameIndex vs BackBufferIndex 两个索引），Vulkan 靠自己组合 |
| `VulkanError::SwapchainOutOfDate` | D3D 无对应错误——`ResizeBuffers` 隐式处理 | Vulkan 把"交换链过时"显式化为可 match 的返回值，重建责任在调用方 |

## 6. 分家后的生长点（步骤 3 起往哪长）

- **管线/描述符/bindless 池**：在 `frames.rs` 的 `record_clear_and_submit` 录制段里生长，同步骨架（2 帧在飞 + 双信号量 + fence）不变；
- **`gpu_allocator`（M2）**：接管内存分配后，`Context` 增一个 allocator 字段即可，Swapchain/FramePool 无感；
- **多窗口（远期）**：Surface 现在因"单窗宿主壳"暂居 `Context`；真要多窗，Surface 升格为与 Swapchain 同级的窗口级类型，`Context` 进一步纯化；
- **命名定案**：`VulkanContext` → `vulkan::Context`（模块担语境，对齐 ash 自家 `khr::surface::Instance` 与 wgpu-hal 惯例）；`swapchain::Swapchain` / `frames::FramePool` 的单词条重复保留；`error::VulkanError` 前缀保留（跨模块自描述）。
