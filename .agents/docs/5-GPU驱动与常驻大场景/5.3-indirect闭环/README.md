# 5.3 indirect 闭环

对应[执行路线图](../../学习目标实现步骤.md)步骤 5 第三段，[段施工计划](施工计划：indirect闭环拆解.md)（2026-10-19 立案，同日施工收官）。本 README 是段任务面板与材料目录。

**定位**：compute 读对象表与本帧可见标记，逐在场行组装 indirect 绘制参数，图形侧一次提交消费——GPU-driven 闭环第三环（收口环）。数据常驻（5.1）、GPU 可见性（5.2）之后，让 CPU 的绘制提交与对象数解耦。FlightHelmet 小场景验正确性。

**一句话说明**：这一段把"画多少、画哪个"的最后一步也交给 GPU——GPU 自己写绘制参数，CPU 只发一次 indirect 绘制。本段不做压缩合批（起步口径已定）、不做排序与透明优化、不做规模压测（归 5.5）。

**当前状态：✅ 施工收官（2026-10-19 当日立案当日施工，判定线五条全过；并回/推送留用户拍板）。**

## 任务面板

- [x] 5.3.1 能力面闸门三件（multi_draw_indirect 启用（新增 enabled_features）/ draw_indirect_count 不启用（count 形状钉明）/ maxDrawIndirectCount 留痕 + 上界执法；真实路径首跑留痕）
- [x] 5.3.2 indirect 命令 compute pass（独立 build_indirect pass；在场环+命令环第五/六族；**施工订正：命令消费屏障 dst 开 DRAW_INDIRECT**）
- [x] 5.3.3 图形提交消费（逐 draw 循环退役 → 一次 indirect 提交；push 4B 退役（firstInstance+instance_index）；DrawCall/DrawList 终态；**施工钉明：在场环方案否决固定槽位全 bound 发命令——幽灵对象盲区**）
- [x] 5.3.4 验证通道与画面级 A/B（egui indirect 读数行 + BRP draw_stats + brp.py 十九子命令；A/B 场景区域零像素差）
- [x] 5.3.5 验收回归（判定线五条全过 + 负例四类 + 回归矩阵：零 VUID ×4 进程、exit 0、clippy 全净、库测试 11/11、resize ×2、三模式基线重留）

## 材料清单

- [施工计划：indirect闭环拆解](施工计划：indirect闭环拆解.md)——段施工计划（2026-10-19 立案）：能力面闸门（立案期订正主计划"无需额外 feature"失真）、任务拆解五项、判定线草案五条、待决六项。
- [施工记录](5.3.1-5.3.5-indirect闭环施工记录：能力闸门、命令组装与图形消费.md)——实际落地：判定线总账、待决六项落定清单、幽灵对象盲区钉明、钉子四枚。
- [indirect闭环流程：从账本行到一次绘制提交的各级账面](indirect闭环流程：从账本行到一次绘制提交的各级账面.md)——流程导读：六族环全景、逐时刻走读、两段式屏障依赖表、失败与稳态路径。
- [indirect闭环机制三问：API边界、dispatch驱动与布局归属](indirect闭环机制三问：API边界、dispatch驱动与布局归属.md)——机制深潜（2026-10-19 收官问答钉）：Vulkan 给了什么（API vs 契约 vs 决策点后移）、dispatch 的自动驱动与独立 pass 取舍、布局归属分界线（规范钉死 vs 私有契约）。
