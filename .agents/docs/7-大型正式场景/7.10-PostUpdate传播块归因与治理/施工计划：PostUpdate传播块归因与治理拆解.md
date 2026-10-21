# 施工计划：PostUpdate传播块归因与治理拆解（7.10）

> 2026-10-21 立案。帧治理四段（7.8~7.11）第三段——归因最粗的一段，先仪器后治理。

> **一句话说明**：本计划把 zero_day 稳态 PostUpdate ~8ms 先逐系统归因（仪器先行，实数说话）再按真凶治理；本计划不主张调度器/ECS 执行器改造、不主张 transform 系统替换（那归引擎侧），治理动作以业务侧为主；归因粗、治理粗的段不做——归因结论必须到"每系统毫秒"级。

## 0. 现状核实（2026-10-21 立案期）

- **PostUpdate ~8ms 属实（实测）**：7.8 立案期临时打点探针（First/PreUpdate/Update/PostUpdate/Last 各调度起点 Instant）测得 post→last 段 ~8.1ms——但探针系统在调度内**无序**（打点触发点漂移），该读数是粗口径，7.10.1 要把它劈到每系统。
- **Compute 池并行烧法属实（实测）**：psutil 按线程名分组采样——Compute Task Pool 8 线程合计 ~46 core-ms/帧（1.6 核当量），main 线程 ~62%；PostUpdate 内的并行系统（候选=transform 传播）与其同窗。
- **逐系统计时仪器缺席（属实，0.20-dev 核实）**：`SystemInformationDiagnosticsPlugin` 只有进程级 CPU/mem 三项；bevy 侧逐系统归因通道 = bevy `trace`/`trace_chrome` 特性（tracing span，退出产 chrome json）或自写有序打点——二选一施工时定。
- **本机 DLP 反调试拦截线程采样（环境钉）**：SuspendThread+GetThreadContext 的 RIP 恒 0（外部剖析法不可用），归因只能走进程内仪器——这也是 7.5 PerfLedger 仪器路线的延续。
- **候选清单（按嫌疑排序）**：①bevy_transform 传播在 16k 节点树的每帧成本（HierarchyDirtyQueue 未脏时也应近零——若不是近零即真凶）；②update_ledger 的 Changed/Removed 查询迭代（processing=0 时应近零）；③bevy_asset/scene 内部系统（handle_internal_asset_events 等）；④bevy_remote tick。真凶以实数裁决，候选清单不作结论。

## 1. 任务拆解

### 7.10.1 归因仪器

- 选型：bevy `trace` 特性（tracing 环境变量过滤，零代码侵入）vs 自写有序打点（调度内显式 `.before/.after` 锚定——7.8 立案期已踩过"无序打点漂移"的坑，打点系统必须显式锚定传播前后）。
- 产出：zero_day 稳态下 PostUpdate 每系统（或每簇）毫秒实数，Sponza 对照（行数梯度）。

### 7.10.2 真凶治理

- 按归因实数逐项治理；候选预案（以实际真凶为准，不预设结论）：
  - 若 transform 传播是主项：查 HierarchyDirtyQueue 是否被某逐帧写路径常脏（如逐帧写 Transform 的系统波及整树）——治"谁在常脏"而非"改传播"。
  - 若账本/资产系统走访是主项：参照 7.9 增量思想收敛。
  - 若执行器/调度开销本身：如实立账（业务侧无治理空间，转 F 挂账或引擎侧议题）。

### 7.10.3 传播正确性与回归

- BRP set-transform 后画面同帧跟随（传播正确性铁线）；拖拽编辑、树窗联动零回退；九脚本抽测 + 库测试 + 零 VUID + exit 0 + clippy 全净。

## 2. 判定线草案

1. **归因证据链**：PostUpdate 每系统（簇）毫秒实数，不靠猜；Sponza 对照钉行数相关性。
2. **治理实数**：治理后同场景 PostUpdate 稳态毫秒对照（目标值治理时按真凶性质定，不硬凑）。
3. **传播正确性**：BRP set-transform 同帧跟随、拖拽编辑零回退（4.1 变更链语义不变）。
4. **回归**：库测试全绿 + 九脚本抽测零回退。
5. **工程闸门**：零 VUID、exit 0、clippy 全净。

## 3. 待决项

- **仪器选型**：bevy trace 特性（依赖编译特性开关，watch 通道要验证）vs 自写有序打点（零依赖但要锚定）——施工时按实现成本钉明。
- **治理范围**：真凶若在 bevy 引擎内部行为（如传播系统设计），业务侧可压则压、不可压则如实立账——不 fork 引擎。
- **与 7.11 的分界**：PostUpdate 段归本段；Last/winit/First 段归 7.11——打点仪器若共用，本段产出可作为 7.11 的仪器底座（一份仪器两段受益）。

## 4. 测试方案

- 取证：归因明细表 + 前后对照，落 `.temp/7.10/`。
- 传播正确性：BRP set-transform + 虚拟鼠标拖拽读回。
- 代码验证线：clippy 全净（禁 cargo fmt）+ 库测试 + 宿主 exit 0。