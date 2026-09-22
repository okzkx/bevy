# 错误处理语法糖：frenderer syntax 与 ash_renderer 移植

> 2026-09-21 沉淀。起因：施工 2.4（步骤 2 帧循环）收官后用户提出"错误处理可以更优雅"，要求总结其为 frenderer 写的宏语法糖（`F:\okzkx\rust-frenderer\modules\common\syntax`，依赖仅 anyhow + log + paste），随后拍板"控制流族全搬"进 ash_renderer。本文 = 糖的完整图谱 + 移植定案 + 使用分界。**错误处理的思想与架构（两 Tier、优雅退出链路）已独立成篇《[错误处理体系：两Tier思想与优雅退出](错误处理体系：两Tier思想与优雅退出.md)》——糖是 Tier① 的工具箱，宪法在那边。**

## 0. 一句话结论

一套"**错误就地消化**"的应用层错误处理风格：anyhow 单类型底座 + 3 个扩展 trait + 8 个控制流宏——错误永远"打日志 + 安全降级继续跑"，不上浮、不 panic；Option 与 Result 用同一套语法消化。ash_renderer 已移植控制流族 + 两个 trait，并新增第 9 件 `unwrap_or_panic!`（补初始化路径的家族位，见 §3/§5）（`ash_renderer/src/syntax.rs`），与 thiserror 类型化错误**互补而非替代**（§6）。

## 1. 底座：anyhow 单类型错误

- frenderer 自有代码**全库零 thiserror、零自定义错误枚举**——`anyhow::Result` 一种错误类型走天下（vendored 的 psd crate 除外）；
- syntax crate 把 `anyhow::{anyhow, Context, Result}` 和 `log::*` 一并再出口，业务 crate `use syntax::*` 即拿到统一词汇表；
- 精神：错误是"值得记一笔的意外"，不是"需要分类的状态"——**分类只在需要分支决策的地方才有价值**（这正是 ash_renderer 里 OUT_OF_DATE 用 thiserror 枚举的原因）。

## 2. 扩展 trait（debug_ext.rs）——糖的"动词"

| trait / 方法 | 语义 | 角色 |
|---|---|---|
| `Result::warn()` | 打 warn 日志后**错误原样穿透**（`map_err` 内 log），可继续 `?` 传播 | 旁路 |
| `Result::info()` | 只打 Debug 首行，同样穿透 | 旁路 |
| `Result::warn_ok()` | warn 后降级成 Option | 出口 |
| `Result::warn_unwrap_or_default()` | warn + `Default` 兜底 | 出口（frenderer `thread_spawn` 线程入口就是它） |
| `Option::some()` | None → `Err(anyhow!("None"))`，Option 升格进 Result 流 | 出口 |

区分要点：**旁路**（错误继续流动，日志只是路过的哨兵）vs **出口**（错误到此为止，值向默认收敛）。

## 3. 控制流宏 8 件——let-else 的模式化

命名规律：**前缀定输入类型，后缀定出口**。

| 前缀族 | 输入 | `_return` 版出口 | 无后缀版出口 | frenderer 业务用量 |
|---|---|---|---|---|
| `unwrap_or` / `unwrap_or_return` | Option | `return 值`（单参版 `return Default::default()`） | 执行发散语句 | 361 / 132 |
| `warn_unwrap_or` / `warn_unwrap_or_return` | Result（先 `.warn()`） | 同上 | 同上 | 124 / 60 |
| `or` / `or_return` | bool | 同上 | 同上 | 74 / 153 |
| `matches_or` / `matches_or_return` | 任意模式 let-else | 同上 | 同上 | 18 / 13 |
| `lock_mutex`（未搬） | Mutex | 中毒 → `anyhow!` 或 return | — | 9 |
| `unwrap_or_panic`（**ash_renderer 新增**） | Result + Option（经 `UnwrapPanic` trait 统一） | `panic!`（双参版拼上下文，单参版裸 panic） | — | frenderer 无此件 |

- let-else 族（unwrap/warn_unwrap/matches）的发散性由**编译器强制**——`else` 分支必须发散，传普通表达式直接编译失败；唯 `or!`/`or_return!` 是 `if !e { stmt }` 形式，发散性不经检查（frenderer 原样保留，ash_renderer 移植版注释里明示只传 `return`/`continue`）；
- 典型形态（frenderer `render_params.rs:35`，逐 element 收集渲染包）：

```rust
let pack = warn_unwrap_or!(render_tool_pack_index_sp_map().get_mut(idx), continue);
```

——缺数据打条日志跳过这个 element，绝不让一帧崩掉。这正是 ash_renderer 步骤 3 逐实体收集绘制数据要写的同款代码。

**设计哲学三点**：①错误就地 warn + 降级继续跑（非 anyhow 正统的向上传播）——对渲染器是对的，丢一帧/缺一个材质不该杀进程；②Option 与 Result 同一套语法消化（`.some()` 升格、`unwrap_or_*` 降格）；③全库零裸 `unwrap()` 零 panic——失败永远是"日志 + 安全默认值"。

## 4. ash_renderer 移植版（`ash_renderer/src/syntax.rs`）

**搬入**：控制流宏 8 件 + `LogDebug`（warn/info/warn_ok）+ `WarnOrDefault`（warn_unwrap_or_default）。另新增 `unwrap_or_panic!` + `UnwrapPanic` trait（Result/Option 统一解包，错误转 String 走 Display）——家族的 panic 位；**现役调用点=无**（工程原则"非必要不 panic"，init 的时序断言也已并入 Tier② 优雅退出，见 §5），仅供真正必要的断言场景备用。frenderer 的初始化路径是裸 match/expect，无此宏。

四个适配（相对 frenderer 原版）：

1. `log` → `bevy::log`（宏名相同，tracing 直换）；
2. 宏内用 `$crate::syntax::LogDebug::warn(...)` **全限定调 trait 方法**——调用方只导宏、免导 trait，宏自包含（frenderel 靠 `use syntax::*` 一揽子导入，我们选择显式）；
3. 全部 `#[macro_export]`（跨 crate 导出的硬要求，漏了报 E0364）+ 模块内 `pub use` 提供 `syntax::宏名` 路径——`ash_renderer::宏名` 与 `ash_renderer::syntax::宏名` 双通道；
4. 模块级 `#![allow(unused_macros)]`——多数宏为 步骤 3+ 预备（逐实体收集 = `unwrap_or!(x, continue)` 主场），当前仅 `warn_unwrap_or_return!` 在 draw_frame 就业。

**未搬三件及理由**：`singleton` 系列（bevy `Resource` 就是全局状态的正解，搬进来反而诱导反模式）；`lock_mutex!`（bevy 调度器管并发，系统内不持手动锁，首个后台线程出现时再议）；`Option::some()`（anyhow `?` 链专用，本 crate 的 Option 早退已由 `unwrap_or_*` 覆盖）。

## 5. 使用分界（用户错误处理思想，两 Tier，**非必要不 panic**）

> 本节思想与 Tier② 优雅退出链路的完整展开在《[错误处理体系：两Tier思想与优雅退出](错误处理体系：两Tier思想与优雅退出.md)》，此处只留工具视角的速查。

用户的工程级失败策略（2026-09-21 明确）——**全部失败只落两层**：

| Tier | 场景 | 处置 | ash_renderer 例 |
|---|---|---|---|
| ① 不影响运行 | 帧循环内可跳过的失败 | **warn 后丢弃，继续运行**（糖家族） | `warn_unwrap_or_return!(frames.wait_and_reset())`、`unwrap_or!(x, continue)`；要业务上下文用显式 `if let`（resize 重建）、要分支决策用显式 `match`（OUT_OF_DATE 分流） |
| ② 影响运行 | 初始化/装配失败（含时序断言） | **Error 冒泡到 main，优雅退出** | `try_init_vulkan` 全程 `?`（`single()` 失败也 `map_err` 冒泡）→ init_vulkan 单点 match：`error!` + `AppExit::error()` → teardown 反序拆除、窗口自关、退出码 1；`draw_frame` 挂 `run_if(resource_exists::<Context>)` 守卫失败帧 |

panic 只在真正"必要"时出场（断言不可恢复的内部不变量且需要 backtrace 取证）——本工程现无此类点位；`unwrap_or_panic!` 作为家族的 panic 位保留备用。**一句话：Tier① 管"这帧算了"，Tier② 管"程序起不来"，panic 不设岗。**

为什么 Tier② 比 panic 强（Vulkan 视角）：panic 的 unwind 析构 App 时对 Resource 的 drop 顺序任意，Swapchain 可能死于 Device 之后（Vulkan 未定义行为）；优雅退出走 teardown 反序拆除（hwnd 还活着时正确销毁 Vulkan），且 bevy 已为 `AppExit` 实现 `Termination`——`fn main() -> AppExit` 退出码自然传出（Error=1）。

## 6. 与 bevy 的关系（为什么糖在 bevy 里比 anyhow 更顺）

- bevy 系统返回 `()`，`?` 主通道**根本不可用**——"warn + 早退"的糖恰好是 `()` 系统里唯一的传播形式；
- bevy 原生备选路：系统返回 `Result<(), BevyError>`，错误交 `ErrorHandler`（`bevy_ecs/src/error/handler.rs:105`，现成 `panic`/`warn` 两实现，`world.set_error_handler` 可换）——解决"panic 还是打日志"，解决不了"OUT_OF_DATE 要重建重试"的分流，只能辅助；
- 最终分工：**thiserror 类型化管边界与分支**（init 失败、OUT_OF_DATE 控制流），**糖管帧循环内非致命失败的就地消化**。

## 7. 用量证据（frenderer 业务代码，grep 全 modules/ 排除 syntax 自身）

`unwrap_or!` 361 ｜ `or_return!` 153 ｜ `some()` 157 ｜ `unwrap_or_return!` 132 ｜ `warn_unwrap_or!` 124 ｜ `or!` 74 ｜ `warn_unwrap_or_return!` 60 ｜ `warn_unwrap_or_default()` 52 ｜ `matches_or*` 31 ｜ `lock_mutex!` 9——卫语句与循环跳元素是绝对主力，matches 系是低频补充。
