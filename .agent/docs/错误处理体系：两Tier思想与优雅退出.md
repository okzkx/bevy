# 错误处理体系：两Tier思想与优雅退出

> 2026-09-21 从《错误处理语法糖》篇独立成篇。分工：**糖篇管工具**（frenderer 宏家族的移植与图谱），**本篇管思想与架构**——用户的错误处理哲学 + ash_renderer 的完整落地。语法糖是 Tier① 的工具箱；本文是整个错误处理体系的宪法。

## 0. 两Tier思想（用户定案，适用于所有工程）

**总原则：非必要不 panic。** 全部失败只落两层：

1. **不影响运行** → warning 警告后丢弃，继续运行；
2. **影响运行** → Error 冒泡到 main，优雅退出。

panic 不设岗——只留给真正"必要"的场景（断言不可恢复的内部不变量、且需要 backtrace 取证），本工程**现无此类点位**。一句话：**Tier① 管"这帧算了"，Tier② 管"程序起不来"，panic 不设岗。**

## 1. 为什么 Tier② 比 panic 强（Vulkan 视角）

这不是审美偏好，是正确性问题：

- **panic 的 unwind 析构 App 时，对 Resource 的 drop 顺序是任意的**——Swapchain 完全可能死在 Device 之后（`destroy_swapchain` 打在已销毁的 Device 上，Vulkan 未定义行为）；
- 优雅退出走 `teardown_vulkan` 反序拆除（hwnd 还活着时正确销毁 Vulkan 对象，顺序=帧级→resize级→进程级）；
- 退出码可传出（`AppExit` 已实现 `Termination`），调用方/脚本可感知失败——panic 的退出码语义丢失；
- 错误消息可拼业务上下文（"Vulkan 初始化失败，宿主壳优雅退出: {根因}"），而非 panic 系统的固定格式。

Unity 映射：Tier① ≈ catch 后 log + 跳过（frame 不会因单个对象失败而中断）；Tier② ≈ 初始化失败时反序 Release 再 return 退出码——引擎不敢崩在用户机器上。

## 2. ash_renderer 落地全景

### 类型层：`VulkanError`（thiserror）

| 变体 | 语义 |
|---|---|
| `SwapchainOutOfDate` | **控制流不是失败**——acquire/present 过时，重建后重试 |
| `Vk(vk::Result)` | 其余 Vulkan 调用失败 |
| `Init(String)` | 初始化链路错误（带调用上下文；时序断言也折叠进来） |

### Tier① 落地（帧循环，warn 丢弃继续）

- 通用兜底：`warn_unwrap_or_return!(frames.wait_and_reset())`——失败打 warn 跳过本帧；
- 循环跳元素：`unwrap_or!(x, continue)`（step3 逐实体收集的主场）；
- 要业务上下文：显式 `if let` + 自写日志（resize 后重建失败）；
- **错误当控制流**：显式 `match`（acquire/present 的 OUT_OF_DATE → rebuild → 重试）——两 Tier 的边界示范：不是所有 Err 都该丢弃，OUT_OF_DATE 的"重试"分支是逻辑本身。

### Tier② 落地（初始化，冒泡到 main 优雅退出）

```
try_init_vulkan（? 串链，一切失败折叠成 VulkanError）
  ├─ wrapper.single() 失败 → map_err 成 Init("...时序假设被打破")
  ├─ Context::new(wrapper)?
  ├─ Swapchain::new(&ctx)?
  └─ FramePool::new(&ctx)?
        ↓ 单点 match（init_vulkan）
Err → error!("Vulkan 初始化失败，宿主壳优雅退出: {e}")
    → exit.write(AppExit::error())（资源全有或全无，一个都不插入）
        ↓ bevy 退出链（同帧 Last 调度内）
teardown_vulkan（OnAppExitSystems）反序拆除：FramePool → Swapchain → Context
        ↓
despawn_windows 销毁 winit 窗口（hwnd 死时 Vulkan 已先死）
        ↓
fn main() -> AppExit（Termination：Success=0 / Error=1）
```

三个配套缺一不可：

| 配套 | 作用 |
|---|---|
| `draw_frame.run_if(resource_exists::<Context>)` | 失败帧没有资源，`Res<Context>` 会 panic——守卫让 Update 静默跳过，优雅退出前功不弃 |
| 资源**全有或全无**插入 | 部分插入会让 draw_frame 半途 panic，破坏两 Tier |
| `fn main() -> AppExit` | bevy 已实现 `Termination`（bevy_app/src/app.rs:1597，**没有** `From<AppExit> for ExitCode`，Termination 是正路），退出码 1 传出 |

## 3. Tier② 能用 `?` 的关键：失败处理集中

bevy 系统返回 `()`，`?` 在系统里不可用——两 Tier 的实现答案不是给系统加返回值，而是**把可能失败的装配抽成返回 Result 的普通函数**（`try_init_vulkan`），系统里只留一个 match。这样：

- 函数内部全程 `?` 传播（含 Option→`map_err` 折叠），失败语境不丢；
- 系统侧单点决策：log + `AppExit` + return；
- 装配逻辑与失败策略解耦，`try_xxx` 函数可复用可测试。

## 4. 实测证据（2026-09-21）

| 路径 | 方法 | 结果 |
|---|---|---|
| Tier② 失败路径 | 伪造实例扩展名（`VK_KHR_bogus_exit_test`）→ `vkCreateInstance` 报 `ExtensionNotPresent` | ✅ 自退、退出码 1、根因日志完整、**零 panic** |
| Tier② 正常路径 | 冒烟运行 | ✅ 全链就绪、清屏循环无回归 |
| 优雅退出（成功路径） | WM_CLOSE | ✅ teardown 反序拆除日志 → 干净退出 |
| Tier① | 帧循环兜底（未人为触发失败，逻辑路径 review） | ✅ 编译期验证 + 行为同 if-let 展开 |

## 5. 给后续的纪律

1. **新代码禁 `panic!`/`unwrap()`/`expect()`**——失败按 Tier 归位：可跳过→糖（Tier①），影响运行→`try_xxx` + 冒泡（Tier②）；
2. 需要"影响运行"的失败时，先问：抽成 `try_xxx` 返回 Result 了吗？守卫（`run_if(resource_exists)`）加了吗？
3. OUT_OF_DATE 类"错误当控制流"是唯一允许的显式 match 失败分支——它是逻辑，不是丢弃；
4. 真要 panic 时（目前不存在此类场景），用 `unwrap_or_panic!`（语法糖家族第 9 件，panic 位）并留下"为什么必须 panic"的注释。
