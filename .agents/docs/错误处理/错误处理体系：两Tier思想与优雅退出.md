# 错误处理体系：两Tier思想与优雅退出

> 2026-09-21 从《错误处理语法糖》篇独立成篇。分工：**糖篇管工具**（frenderer 宏家族的移植与图谱），**本篇管思想与架构**——用户的错误处理哲学 + ash_renderer 的完整落地。语法糖是 Tier① 的工具箱；本文是整个错误处理体系的宪法。
>
> **2026-10-10 修订**：Tier② 收尾路线改走官方统一错误通道（`tier_handler` 按 Severity 分流 + `fatal_gate` 看门写 `AppExit`，见 §3 与定案篇《代码结构优化：统一错误通道定案》）；两 Tier 思想（§0）与非必要不 panic 不变。

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

### Tier① 落地（不进通道，就地消化）

- 循环内逐项降级：转换拒绝/规格拒绝/材质未到货 → 就地 warn（一次去重）+ continue，脏行下帧重试（upload.rs §2）；
- **错误当控制流**：显式 `match`（acquire/present 的 OUT_OF_DATE → rebuild → 重试）——不是所有 Err 都该丢弃，OUT_OF_DATE 的"重试"分支是逻辑本身；
- 不提前 return 的裁决就地做：present 真失败 → 就地 error + `request_graceful_exit()`（advance 必须照常跑保持帧槽账本一致）；
- 状态告知类（RenderMode 缺席、相机缺席 identity 兜底）→ 就地 warn 一次去重。

### Tier② 落地（提前 return 的失败，冒泡统一通道）

```
try_init_vulkan（? 串链，一切失败折叠成 VulkanError）
  ├─ wrapper.single() 失败 → map_err 成 Init("...时序假设被打破")
  ├─ Context::new(wrapper)?
  ├─ Swapchain::new(&ctx)?
  └─ FramePool::new(&ctx)?
        ↓ init_vulkan：.context("Vulkan 初始化失败，宿主壳优雅退出")? 冒泡
（错误点不打 log；资源全有或全无，一个都不插入）
        ↓ 统一错误通道
tier_handler：error 留档（带 system 名与负载）+ 置位 FATAL → fatal_gate 写 AppExit
        ↓ bevy 退出链
teardown_vulkan（OnAppExitSystems）反序拆除：FramePool → Swapchain → Context
        ↓
despawn_windows 销毁 winit 窗口（hwnd 死时 Vulkan 已先死）
        ↓
fn main() -> AppExit（Termination：Success=0 / Error=1）
```

上传链（`flush_uploads`）同形状：转换/规格/材质未到货就地降级，容量保证/建图/提交/发布/驻留登记提前 return 的失败 `or_fatal` 冒泡（见 upload.rs）。

三个配套缺一不可：

| 配套 | 作用 |
|---|---|
| `draw_frame.run_if(resource_exists::<Context>)` | 失败帧没有资源，`Res<Context>` 会 panic——守卫让 Update 静默跳过，优雅退出前功不弃 |
| 资源**全有或全无**插入 | 部分插入会让 draw_frame 半途 panic，破坏两 Tier |
| `fn main() -> AppExit` | bevy 已实现 `Termination`（bevy_app/src/app.rs:1597，**没有** `From<AppExit> for ExitCode`，Termination 是正路），退出码 1 传出 |

## 3. Tier② 能用 `?` 的关键：统一错误通道（2026-10-10 施工，替代壳+body）

system 签名 `Result<(), BevyError>`，`?` 冒泡的 Err 由执行器交给统一 handler
（`common::error::tier_handler`，`App::set_error_handler` 注册），按错误值的
`Severity` 元数据两 Tier 分流：

- `Warning` → handler 就地 warn 丢弃（Tier①，下帧自愈）；
- 其余 → error 留档 + 置位 `FATAL: AtomicBool`，`fatal_gate` 看门 system（Update
  早位）写 `AppExit` 优雅退出（Tier②）——handler 是裸函数指针、无 world 访问权，
  退出请求只能经静态位中转；
- 错误消息用 `.context(...)` 挂进错误值，日志在 handler 统一打（错误点不再 log，
  单点零重复）；**冒泡与否只看"是否要求提前 return"**——循环内逐项降级、时序
  过程记录就地 log，不进通道。

一次性装配仍走 `try_init_vulkan` 串链，系统侧挂 context 负载 `?` 冒泡（同通道同裁决）。
统一通道的机制事实与坑钉正本见定案篇《代码结构优化：统一错误通道定案》。

旧答案（壳+body，已退役）：主体拆成返回 `Result` 的普通函数、system 壳 match
收尾——可行但收尾逻辑每个 system 各写一遍壳，无真正统一。

## 4. 实测证据

### 壳模式时代（2026-09-21，机制已退役、证据仍有效）

| 路径 | 方法 | 结果 |
|---|---|---|
| Tier② 失败路径 | 伪造实例扩展名（`VK_KHR_bogus_exit_test`）→ `vkCreateInstance` 报 `ExtensionNotPresent` | ✅ 自退、退出码 1、根因日志完整、**零 panic** |
| Tier② 正常路径 | 冒烟运行 | ✅ 全链就绪、清屏循环无回归 |
| 优雅退出（成功路径） | WM_CLOSE | ✅ teardown 反序拆除日志 → 干净退出 |
| Tier① | 帧循环兜底（未人为触发失败，逻辑路径 review） | ✅ 编译期验证 + 行为同 if-let 展开 |

### 统一通道版（2026-10-10）

| 路径 | 方法 | 结果 |
|---|---|---|
| Tier② 失败路径 | 同伪造扩展名 → `init_vulkan` 冒泡 | ✅ handler error 留档带 system 名与负载（`System 'init_vulkan' failed: Vulkan 初始化失败…: vkCreateInstance 失败…`）→ fatal_gate 写 AppExit → 优雅拆除、退出码 1、**零 panic**；失败帧本帧剩余 system 照跑（半帧延迟语义实测） |
| Tier② 正常路径 | 冒烟运行（上传链两批次 + 增量清账） | ✅ 全链就绪、零 ERROR/WARN 误报 |
| Tier①（不进通道） | 转换拒绝/未到货/相机缺席等就地降级 | ✅ warn 一次去重，帧循环照常 |

## 5. 给后续的纪律

1. **新代码禁 `panic!`/`unwrap()`/`expect()`**——失败按 Tier 归位：可跳过→就地 log/糖（Tier①），影响运行→`?` 冒泡统一通道（Tier②）；一次性装配走 `try_xxx` 串链 + 系统侧 `?` 冒泡；不变式断言改 Err（BRP 侧回协议错误）；
2. 需要"影响运行"的失败时，先问：system 签名 `Result<(), BevyError>`（或抽 `try_xxx`）了吗？守卫（`run_if(resource_exists)`）加了吗？
3. OUT_OF_DATE 类"错误当控制流"是唯一允许的显式 match 失败分支——它是逻辑，不是丢弃；
4. 真要 panic 时（目前不存在此类场景），用 `unwrap_or_panic!`（语法糖家族第 9 件，panic 位）并留下"为什么必须 panic"的注释。
