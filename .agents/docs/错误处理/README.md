# 错误处理专题

跨步骤的错误处理体系专题，三篇一体，另有一份可执行规范；代码锚点在 `ash_renderer/src/common/`（`error.rs` 统一通道基建 + `syntax.rs` 语法糖）。

## 篇目与读序

| 篇 | 文件 | 定位 |
|---|---|---|
| 宪法篇 | [错误处理体系：两Tier思想与优雅退出.md](错误处理体系：两Tier思想与优雅退出.md) | 思想与架构：两 Tier（非必要不 panic）、Tier①/② 落地全景、统一错误通道（§3）、实测证据与后续纪律 |
| 工具篇 | [错误处理语法糖：frenderer syntax与ash_renderer移植.md](错误处理语法糖：frenderer syntax与ash_renderer移植.md) | frenderer 宏糖全图谱与移植定案：控制流宏 9 件、`LogDebug`/`ResultTierExt` 两 Tier 分流、与 bevy 的分工 |
| 定案篇 | [代码结构优化：统一错误通道定案.md](代码结构优化：统一错误通道定案.md) | 统一错误通道设计与落地清单：`tier_handler` + `FATAL` + `fatal_gate` 基建、判定线与坑钉 |
| 规范 | [`../../rules/System错误处理规范.md`](../../rules/System错误处理规范.md) | 可执行约束：签名/判定线/Severity 契约/糖家族/禁令（rules 层，代码评审按它对表） |

读序：先宪法（思想），再定案（现行机制与施工进度），糖篇按需查工具用法，写码对表查规范。

## 状态

- 2026-10-10 定案同日开工：基建三件 + `draw_frame` 拆壳落地并冒烟；
- 2026-10-10 全库推广：`upload.rs` 十处、`init_vulkan`、`frames.rs` 两处 panic、BRP 不变式 expect 全部收敛到统一通道，方案升格为规范（rules 层）—— Tier② 就地裁决与 AppExit 写点在业务 system 里已绝迹。
