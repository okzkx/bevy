# 7.11 帧残余区归因收账（M6 追加段）

对应[步骤 7 README](../README.md)与[路线图步骤 7 节](../../学习目标实现步骤.md)。

**目的**：帧治理四段的收账段——`frame_wall_ms − cpu_total_ms` 缺口（Last+winit 区 ~8ms + First/PreUpdate ~1.5ms，2026-10-21 实测）目前只有粗口径，本段把它劈细：执行器开销、winit 事件胶水、draw_frame 各段之外的驱动侧成本逐项归因，可修小项顺手修，结构级大项如实立账。

**一句话说明**：本段把 frame_wall − cpu_total 的未计时区按段劈细归因（Last 调度内 draw_frame 之外的部分、winit/执行器胶水、First/PreUpdate），可修小项顺手修、结构级大项写明去向；本段不主张重写帧循环结构（runner 形态是 2.x 宿主壳的定案），F26 挂账随本段裁决关闭或转正。

**当前状态：✅ 施工收官（2026-10-21 当日立案当日施工）：缺口全账劈细（大头 = 60Hz 呈现节拍合法空闲 + 调度工 ~7ms 逐系统列账）；UiPlugin 禁用顺手修；F26 关闭。**[施工记录](7.11.1-7.11.3-帧残余区归因收账施工记录：缺口列账、零消费者插件禁用与F26裁决.md)。

## 任务面板

| 任务 | 内容 | 状态 |
|---|---|---|
| 7.11.1 | 残余区劈细（frame_wall − cpu_total 缺口按段列账；draw_frame 内 assemble 之外的段复核） | ✅ frame_wall 序列化进 perf_stats/perf_samples；trace 调度窗口包含归属法（7.10 仪器复用），调度×系统列账表入施工记录 |
| 7.11.2 | 可修小项治理 + 结构级大项立账 | ✅ UiPlugin 禁用（~0.1ms 零消费者）；gizmo/picking 实摘受挫转立账；update_ledger/mark_dirty_trees/animate_targets/calculate_bounds 引擎侧去向写明 |
| 7.11.3 | F26 裁决（关闭或转正写明去向）+ 回归 | ✅ F26 关闭（证据链：节拍空闲 + 调度工已逐段归主）；六脚本 parity、库测试 34/34、零 VUID、exit 0、clippy 净 |

## 材料清单

- [施工计划：帧残余区归因收账拆解](施工计划：帧残余区归因收账拆解.md)——2026-10-21 立案：现状核实、任务三项、判定线草案四条、待决三项。