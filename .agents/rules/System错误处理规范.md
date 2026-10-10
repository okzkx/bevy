# System 错误处理规范

> 适用范围：ash_renderer 全部 bevy system（observer/command 顺带同通道受益）。机制知识与设计取舍的正本见专题《[错误处理体系：两Tier思想与优雅退出](../docs/错误处理/错误处理体系：两Tier思想与优雅退出.md)》与《[代码结构优化：统一错误通道定案](../docs/错误处理/代码结构优化：统一错误通道定案.md)》；本文是可执行约束，与代码不一致时以代码+正本为准并回来修本规范。

## 0. 判定线（唯一）

**冒泡与否只看一件事：本次失败是否要求提前 return。**

- **要提前 return → 冒泡**：错误点挂 context 负载后 `?` 上交通道，错误点不打 log——handler 统一打（单点日志零重复，`ErrorContext` 白送出错的 system 名）；
- **不提前 return → 就地 log，不进通道**：循环内逐项降级（转换拒绝、材质未到货，log 后 continue——冒泡的物理语义是放弃整批剩余项）与强时序性过程记录（重试计数、让路数量，离开现场时间线就没有意义）属这层。

## 1. System 签名

- 失败要冒泡的系统：签名 `Result<(), BevyError>`。错误类型**必须**是 `BevyError`——返回 `Result<T, 自定义E>` 时整个 Result 被当普通值，Err 静默吞（编译照过）；
- 无失败路径的系统：保持 `()`，就地控制流走宏族（`unwrap_or!`/`warn_unwrap_or!`/`matches_or!`，见语法糖篇）；
- **禁** system 参数表出现 `MessageWriter<AppExit>`——写 AppExit 是看门 system 的专属职责，业务 system 只管冒泡。

## 2. Severity 契约（tier_handler 按 severity 分流）

| Severity | 语义 | 产出方式 |
|---|---|---|
| `Warning` | Tier①：影响不大，下帧自愈 | `or_warn(msg)` |
| `Error` | Tier②：状态不可信，优雅退出 | `or_fatal(msg)`、`fatal(msg, err)`、`bevy_error!(Severity::Error, ...)` |
| panic 岗 | 真 panic（带 payload）原样 `resume_unwind`，保 backtrace 取证 | 不主动构造；handler 必须保留该分支 |

- 忘标 severity 的错误默认 `Severity::Panic`（无 payload）→ handler 兜底按 Tier② 走——**不要依赖兜底，Tier② 必须显式标注**；
- BRP 自定义方法 handler 不走本通道（协议层错误回 `BrpError`，如 `BrpError::internal`），但同样禁 panic。

## 3. 语法糖家族（`common::syntax`）

- `or_warn(msg)` / `or_fatal(msg)`：Result 链专用，一行完成"挂 context 负载 + 标级"，`?` 冒泡；
- `fatal(msg, err)`：match 分支/裸错误值就地构造（msg 可 `format!` 带变量）；
- `bevy_error!(Severity::Error, "fmt", args)`：无底层错误值的判空矛盾/内部矛盾（let-else、`ok_or_else`）；
- 坑：泛型代码里 `map_err(BevyError::from)` 解析不了 blanket From（bound 只有 `Into<BevyError>`），写 `map_err(|e| e.into())`。

## 4. 禁令

- 禁 `panic!`/`unwrap()`/`expect()`——不变式断言改返回 Err（系统侧 Tier②、BRP 侧 `BrpError::internal`），宁可协议报错不炸宿主；
- 禁就地 `error!` 后再冒泡（双 log）；禁就地 `error!` 后继续跑（状态不可信必须冒泡，退出裁决在看门）；
- 禁把循环内逐项降级改写成冒泡（`?` 放弃整批剩余项，语义就错了）；
- 判空矛盾类（"XX 后 YY 必在"）不是断言岗，是 Tier② 冒泡岗。

## 5. 机制（一次性注册，勿动）

- main.rs：`App::set_error_handler(tier_handler)` + `add_systems(Update, fatal_gate)`（gate 为 Update 首个注册系统=早位；语义上任意位置都正确，早位只省致命帧的废活）；
- 实现住 `ash_renderer/src/common/error.rs`（`tier_handler` + `FATAL: AtomicBool` + `fatal_gate` + `request_graceful_exit`）；
- 全局静态是进程级：多 App 并存会串扰——本项目单进程单 App，前提钉死；
- 置位到退出有半帧延迟：FATAL 置位后本帧剩余 system 照跑，runner 帧末查 AppExit 才停（与壳模式等价）；
- 参数校验失败的分流（0.20-dev 实钉）：可跳过校验缺席 = `RunSystemError::Skipped`，静默跳过不进通道；**必选参数（如 `Res<T>`）缺席 = `Failed` 进通道**——按 Tier② 处理是正确语义（资源缺席=状态不可信），正常路径由 `run_if(resource_exists)` 守卫兜住。
