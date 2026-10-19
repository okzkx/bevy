# 5.3 indirect 闭环

对应[执行路线图](../../学习目标实现步骤.md)步骤 5 第三段，[段施工计划](施工计划：indirect闭环拆解.md)（2026-10-19 立案，未施工）。本 README 是段任务面板与材料目录。

**定位**：compute 读对象表与本帧可见标记，逐槽组装 indirect 绘制参数，图形侧一次提交消费——GPU-driven 闭环第三环（收口环）。数据常驻（5.1）、GPU 可见性（5.2）之后，让 CPU 的绘制提交与对象数解耦。FlightHelmet 小场景验正确性。

**一句话说明**：这一段把"画多少、画哪个"的最后一步也交给 GPU——GPU 自己写绘制参数，CPU 只发一次 indirect 绘制。本段不做压缩合批（起步口径已定）、不做排序与透明优化、不做规模压测（归 5.5）。

**当前状态：⬜ 已立案（2026-10-19），未施工。**

## 任务面板

- [ ] 5.3.1 能力面闸门三件（multi_draw_indirect / draw_indirect_count 视 count 形状 / maxDrawIndirectCount 留痕）
- [ ] 5.3.2 indirect 命令 compute pass（读对象表 + 标记，逐槽写命令，不可见 `instanceCount=0`）
- [ ] 5.3.3 图形提交消费（逐 draw 循环退役，一次 indirect 提交；push 退役与 DrawList 终态待决钉明）
- [ ] 5.3.4 验证通道与画面级 A/B（剔除开关 on/off 画面一致 + draw-stats 读数）
- [ ] 5.3.5 验收回归（判定线全量 + 负例 + 回归矩阵）

## 材料清单

- [施工计划：indirect闭环拆解](施工计划：indirect闭环拆解.md)——段施工计划（2026-10-19 立案）：能力面闸门（立案期订正主计划"无需额外 feature"失真）、任务拆解五项、判定线草案五条、待决六项。
