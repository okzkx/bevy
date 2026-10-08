# 施工 3.8：system 标注宏

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 收官后追加段（用户 2026-10-08 增补）。主产出为[施工记录](3.8.1-system标注宏施工记录：透明标注与编译期校验.md)——`ash_macros` crate 的 `#[system]` 透明标注宏、compile-fail 五负例与宿主全量标注。

**状态：✅ 已收官（2026-10-08）。**宿主 17 个具名系统全部标注，compile-fail 五负例全拒，clippy 三 crate 全净，宿主实跑回归零变化（exit 0 / 零 VUID / 判定线零回退）。

**目的**：给系统函数打阅读路标——Bevy 的系统没有任何标记（任何满足签名的函数在 `add_systems` 调用点被 `IntoSystem` 编译期转换），读代码时"哪些函数是系统"只能反查注册点。`#[system]` 把这个身份前置到函数定义处，并在编译期钉住系统的一条铁律（不能带 `self`——`System` 无业务 self，方法标注系统要到 `add_systems` 处才以难读的 trait 错误失败）。

**定案边界**（用户 2026-10-08 拍板）：宏**不是**为了自动收集——Bevy 并发调度靠显式 ordering，自动收集系统会失去排序控制权，官方也不走这条路。宏是透明的：函数 token 原样保留，`add_systems` 调用点与运行时行为零变化；属性参数位预留作后期元数据扩展，当前带参在编译期拒绝（防"看着生效、没人读取"的失真标注）。

## 任务清单

- [x] **3.8.1 透明标注宏与全量标注**：`ash_macros`（proc-macro，`#[system]` 透传 + 两条语法铁律校验：只能标注函数、不能带 self）+ `ash_macros/compile_fail`（`ui_test` 五负例：三种 self 方法、带参、非函数）+ 宿主 17 个具名系统全量标注（`driver` 4、`scene/mechanism` 3、`scene/content` 7、`overlay` 2、`announce` 1）。收账：compile-fail 5/5、clippy 三 crate `--all-targets` 全净、宿主实跑 WM_CLOSE exit 0 / 零 VUID / 到货核验 6/6 零回退。排障钉子：ui_test `DependencyBuilder` 对**无构建目标**的 compile-fail crate 拿不到 proc-macro 依赖产物（cargo 跳过无消费者目标的 proc-macro），占位 lib 重出口宏修复，实录见[施工记录 §3](3.8.1-system标注宏施工记录：透明标注与编译期校验.md)。

## 判定线

1. **标注零行为变化**：实跑宿主，WM_CLOSE 优雅退出 exit 0、零 VUID、拆除序完整、场景到货核验 6/6 零回退。✅（2026-10-08 实测）
2. **编译期校验生效**：compile-fail 五负例全拒。✅（5/5 过）
3. **透明性**：带标注的宿主编译通过且 `add_systems` 调用点无任何改动；clippy `ash_macros` / `ash_macros_compile_fail` / `ash_renderer` 三 crate `--all-targets` 全净。✅
4. **不做参数白名单**：`SystemParam` 是开放集合，签名合法性仍由 `add_systems` 调用点 trait 检查；宏只钉语法铁律。✅（设计如此，见宏 crate 文档注释）

## 待决问题

- 无。属性参数（元数据扩展位）未实现属预留设计，不立账；闭包系统（如 `run_if` 内联 closure）无法标注，不在范围内。
