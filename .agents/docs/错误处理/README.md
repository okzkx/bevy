# 错误处理专题

跨步骤的错误处理体系专题，三篇一体；代码锚点在 `ash_renderer/src/common/`（`error.rs` 统一通道基建 + `syntax.rs` 语法糖）。

## 篇目与读序

| 篇 | 文件 | 定位 |
|---|---|---|
| 宪法篇 | [错误处理体系：两Tier思想与优雅退出.md](错误处理体系：两Tier思想与优雅退出.md) | 思想与架构：两 Tier（非必要不 panic）、Tier①/② 落地全景、统一错误通道（§3）、实测证据与后续纪律 |
| 工具篇 | [错误处理语法糖：frenderer syntax与ash_renderer移植.md](错误处理语法糖：frenderer syntax与ash_renderer移植.md) | frenderer 宏糖全图谱与移植定案：控制流宏 9 件、`LogDebug`/`ResultTierExt` 两 Tier 分流、与 bevy 的分工 |
| 定案篇 | [代码结构优化：统一错误通道定案.md](代码结构优化：统一错误通道定案.md) | 统一错误通道设计与落地清单：`tier_handler` + `FATAL` + `fatal_gate` 基建、判定线与坑钉 |

读序：先宪法（思想），再定案（现行机制与施工进度），糖篇按需查工具用法。

## 状态

- 2026-10-10 定案同日开工：基建三件 + `draw_frame` 拆壳落地并冒烟（渲染零 ERROR/WARN 误报、WM_CLOSE 优雅退出 exit 0、clippy 全净），详见定案篇落地清单；
- 待续：`driver/upload.rs` 九处 Tier② 样板收成 `?` 冒泡（定案篇 §6.3）。
