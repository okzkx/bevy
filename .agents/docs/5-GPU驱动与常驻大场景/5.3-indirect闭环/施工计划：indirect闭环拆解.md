# 施工计划：indirect闭环拆解（5.3）

> 2026-10-19 立案（用户指令，纯文档立案），未施工。对应[路线图](../../学习目标实现步骤.md)步骤 5 第三段与[步骤 5 施工计划](../施工计划：最小GPU驱动闭环与常驻大场景五段拆解.md) §5.3 节：GPU 写 indirect 绘制参数、图形提交消费，GPU-driven 闭环的第三环（收口环）——数据常驻（5.1）、GPU 可见性（5.2）之后，让 CPU 的绘制提交与对象数解耦。本文管 5.3 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。段任务面板随开工转[段 README](README.md)。
>
> **一句话说明**：这一段把"画多少、画哪个"的最后一步也交给 GPU——compute 读对象表与本帧可见标记，逐槽组装 indirect 绘制命令（不可见 `instanceCount=0`），图形侧一次 `cmd_draw_indexed_indirect` 消费，绘制调用数从此不随在场对象数增长。本段以 FlightHelmet 小场景验正确性，给 5.4 常驻大场景铺"规模上去了 CPU 提交不动"的底；不做压缩合批（起步口径已定）、不做排序与透明优化、不做规模压测（归 5.5）。
>
> 场景口径沿用步骤定案：本段以 FlightHelmet 小场景验正确性（spawn 少量对象铺视锥内外样本），规模档位读数归 5.5。

## 0. 判定线（验收草案）

1. **CPU 绘制调用数固定不随 N 增长**：录制层一次 `cmd_draw_indexed_indirect`（count 形状见待决 2）替代逐 draw 循环——BRP spawn/增删对象前后，录制层绘制调用读数恒定（小场景读数留证；1 万/5 万/10 万档读数归 5.5）。
2. **不可见对象不产生 GPU 工作 + 画面级 A/B**：标记 = 不可见的槽，对应命令 `instanceCount=0`；剔除开关 off/on 的**画面级** A/B 一致——只被剔除的都不可见，其余逐像素不变（2026-10-15 拍板随 indirect 消费归本段，5.2 只验过标记数据层）。
3. **红线：可见集不回读驱动绘制**（5.2/5.3 共同红线，主计划 §2 原文）。边界钉明：CPU 传给 indirect 的命令条数上界来自账本 bump bound（CPU 在账信息，与 cull dispatch 上界同源），**不是**可见集回读；诊断性计数回读照 5.2 拍板只进计量、单独标注。
4. **5.1/5.2 判定线零回退**：对象增量稳态零上传零盘点、材质环/模型环写入语义、cull-stats 对照零 mismatch 语义、槽位闭环、标记环 fence 纪律。
5. **常规回归**：零 VUID（含 indirect 命令路径）、WM_CLOSE exit 0、clippy 全净、3.5 三模式截图零回退（indirect 路径下重留基线——剔除关闭时与旧逐 draw 路径逐像素一致，是"数据通路等价、只换提交形状"的证据）、resize/最小化与 compute 资源共存、拆 Device 排空不炸。

## 1. 定位：现状与要换掉的东西

5.2 收官后的绘制链：标记环已产出（set2 b3，COMPUTE-only，HOST_VISIBLE ×2 帧槽，u32/槽；`vulkan/instance_table.rs` 模块注释已预告"5.3 GPU 消费进来时评估迁 DEVICE_LOCAL"）；cull dispatch 记录在图形命令缓冲内，消费预置屏障已把 dst stage 开到 `VERTEX_SHADER | COMPUTE_SHADER`（`vulkan/cull.rs:262`）——图形读与"后续 compute 链式读"两个消费方向都已覆盖，本段不新增屏障形状。

绘制侧仍是 5.1 形状：DrawList = CPU 槽位枚举（`driver/host/drawlist.rs` assemble，O(N) 账本走访），录制层逐 draw push 4B 实例槽号 + `cmd_draw_indexed`（`vulkan/frames.rs:435`）——绘制调用数与 push 字节都 O(N)。本段换掉的是这个"逐 draw 循环"。

**数据源头已就位（本段不新建表）**：对象记录 48B 带全三个池引脚 `first_index`/`index_count`/`vertex_base`（`vulkan/instance_table.rs:62` `ObjectRecordLayout` + offset 断言钉死）——GPU compute 逐槽组装 `VkDrawIndexedIndirectCommand` 所需的全部字段已在显存里，命令组装 pass 读 set2 三表 + b3 标记即可写命令；图形 shader 预计零改动（仍按实例索引读 b0/b1/b2，不读标记）。

**CPU 走访如实计量**：材质环全量重写的输入（`DrawAssembly.materials`）仍来自账本 O(N) 走访——红线封的是"可见集回读"与"绘制调用线性增长"，不是 CPU 常驻环写侧的走访；判定线 1 只对绘制调用数与 push 字节划线，走访行数如实进计量（口径辨析见待决 5）。

## 2. 前置闸门：indirect 能力面三件（过闸才开主体）

主计划 §5.3 写"drawIndirectCount 为 1.2 core、multiDrawIndirect 为 1.1 core，进程已请求 API 1.3，无需额外 feature"——**立案期核实订正：不属实**。两条命令各挂着仍须显式启用的 feature 位，"支持≠启用"纪律照旧（VUID 只有验证层执法）：

| # | 闸门 | 现状核实（立案时点） | 实测动作 |
|---|---|---|---|
| 1 | `multi_draw_indirect`（base features 位） | 挂在 `VkPhysicalDeviceFeatures`（ash `definitions.rs:6300`，1.0 起就是 feature 位，不是"1.1 core 无位"）；当前 `DeviceCreateInfo` 未传 `enabled_features`（`context.rs:386`，base 位全 0）——**未启用，用即违规** | 查询留痕 → 设备创建启用 → 最小空转首跑（host 写一条 `instanceCount=1` 命令，`cmd_draw_indexed_indirect` 零 VUID） |
| 2 | `draw_indirect_count`（Vulkan12Features 保留位） | 1.2 core 晋升但 feature 位未取消（ash `definitions.rs:31380` 在 `PhysicalDeviceVulkan12Features`），未启用时调 `cmd_draw_indexed_indirect_count` 违 VUID；当前 vk12 启用面 6 位（`context.rs:343`） | count 形状路线（待决 2）选中才启用；启用流程照 timeline/dynamicRendering 先例：支持查询 → 硬校验 → 显式启用 → 就绪日志登记 |
| 3 | `limits.maxDrawIndirectCount` 留痕 | 1.1 起 limits（ash `definitions.rs:6775`），规范最小值仅 1；固定槽位满容量命令条数（131072 上界）须实测读数 | 就绪日志登记读数；不足则上界形状改走 count buffer 路线 |

闸门产物（查询读数、启用面变更、空转首跑证据）直接进 5.3 施工记录，不另立文档。

## 3. 任务拆解草案（开工时转段 README 任务面板）

| 任务 | 内容 | 验收锚点 |
|---|---|---|
| 5.3.1 能力面闸门三件 | §2 全表：查询留痕、feature 启用、limits 读数、空转首跑 | 判定线 5 留痕 |
| 5.3.2 indirect 命令 compute pass | 读对象表 + 本帧标记，逐在场槽写 `VkDrawIndexedIndirectCommand`（firstInstance 携带实例槽号，待决 3；不可见 → `instanceCount=0`）；命令 buffer = 新增 DeviceStorage 帧槽环（×2，照标记环链，不新增等待闸门）；pass 形状待决 1（独立 pass vs cull 顺带写） | 命令与标记逐槽对照可测 |
| 5.3.3 图形提交消费 | 录制层逐 draw 循环退役 → 一次 indirect 提交（count 形状待决 2）；push 4B 退役与否（待决 3）；DrawList 终态（待决 5）；空场/pending_rows 口径（待决 6） | 判定线 1 读数 |
| 5.3.4 验证通道与画面级 A/B | 剔除 A/B 开关从标记数据层升级画面级；draw-stats 读数（绘制调用数、命令条数、`instanceCount=0` 计数——诊断标注）；egui 挂现有面板 + BRP 读数随需扩 | 判定线 1/2/3 证据成形 |
| 5.3.5 验收回归 | 判定线全量 + 负例（空场、全不可见、单对象、despawn 后槽位复用不串、resize 途中、拆 Device）+ 回归矩阵 | §0 全过 |

顺序理由：闸门先过（feature 未启用则 indirect 调用直接违 VUID），命令 pass 次之（有标记可读、有命令可写），提交消费再后（有命令可消费），验证通道最后（画面级 A/B 需要消费先成立）。5.3.2~5.3.3 是本段主体。

**与冻结边界的关系**：对象表/材质表记录布局不动（5.1 已定）；base features 启用面扩位（`multi_draw_indirect` 必启，`draw_indirect_count` 视待决 2）——就绪日志照 5.2 先例同步登记；set2 描述符声明预计零改动（图形 shader 仍只读 b0/b1/b2，标记消费在新 compute 侧，b3 COMPUTE-only 声明恰好够用）；命令 buffer 是新增 storage binding，落位选择施工时钉明。

## 4. 测试方案与证据形态

- **BRP 驱动**（既有方法族全在库）：spawn/despawn/replace 前后绘制调用读数对照（判定线 1）；set_transform 移动对象出入视锥 → 画面级 A/B 逐像素对照（判定线 2）；cull-stats/cull-set 照旧（brp.py 十八子命令起步，draw 读数子命令随需扩，不写一次性探针）。
- **计量分报**：录制层绘制调用数（应恒 1 次 API 调用）、GPU 侧命令条数、`instanceCount=0` 条数（诊断标注）、CPU 账本走访行数（O(N) 走访仍在，如实报告不藏）；egui 挂 3.7/4.7 现有面板。
- **截图读图**（按[画面功能视觉验证](../../../rules/画面功能视觉验证.md)）：三模式 indirect 路径基线重留 + 画面级 A/B（同机位开关对照裁剪放大复核）。
- **负例**：空场（count=0 路径不 crash、不产生绘制）；全部不可见帧（清屏画面、零 GPU 绘制工作读数）；despawn 后槽位复用——旧命令不得串到新对象（槽位与命令同界对照）；窗口 resize/最小化与命令 buffer 生命周期共存；拆 Device 排空（命令 buffer 进停尸房口径照标记环先例）。
- **回归矩阵**：4.x 全量判定线、5.2 cull-stats 对照、3.5 三模式、零 VUID（含 indirect 命令路径与验证层开启）、WM_CLOSE exit 0、clippy 全净、增删后 resize×2 不炸。

## 5. 待决问题（施工时钉明，不写死）

1. **命令组装 pass 形状**：独立 build_indirect compute pass（Bevy `mesh_preprocess` → `build_indirect_params` 两步同构，依赖边界清楚，cull 管线布局不动——5.2 首钉"组号随管线布局"的教训）vs cull.wgsl 顺带写命令（省一次 dispatch 与一次读表，两个写职责挤一个 shader）。倾向独立 pass，施工时评估后钉明。
2. **count 形状**：`cmd_draw_indexed_indirect`（CPU 传 bump bound 上界，`multi_draw_indirect` 位即够）vs `cmd_draw_indexed_indirect_count`（count buffer GPU 写，须启用 `draw_indirect_count` 位）——倾向前者起步：固定槽位下命令条数上界是 CPU 在账信息（非可见集），少启用一个 feature 位、少一个 GPU 写 buffer；翻案条件 = 出现"实际命令条数只有 GPU 知道"的形状（压缩合批、5.4 规模化）。
3. **push 4B 实例槽号退役**：`firstInstance` 携带槽号、shader 用 `instance_index` 读 set2（push 彻底退役，5.1 遗志收尾）vs 保留 push——倾向退役；连带核实 WGSL `instance_index` 语义与 `firstInstance` 的映射（`draw_indexed` 的 base_instance 语义与现有 push 值域）。
4. **标记环迁 DEVICE_LOCAL**（模块注释预告项）：GPU 消费后 host 侧只剩诊断读数——保持 HOST_VISIBLE（零改动，独显 BAR 可放；读数零成本）vs 迁 DEVICE_LOCAL（诊断读数走低频 staging 拷贝）。倾向本段不动，5.5 实测数据再定；本段只把"迁与不迁"的判据（标记环访问频率、BAR 容量读数）留痕。
5. **DrawList 终态与红线口径**：`draws` 数组预计退役（录制层不再需要 CPU 侧逐对象参数）；材质环全量重写输入仍走账本 O(N) 走访——红线口径钉明：走访组装常驻环写侧**允许**（它不是 draw list 重建、不是可见集回读），判定线 1 只封绘制调用数与 push 字节；"CPU 不逐对象重建 draw list"的判定以录制层代码面为准。
6. **空场与暂缓行**：count=0 时 indirect 提交的合法形状（`cmd_draw_indexed_indirect` 传 0 条是否合法、与现有"DrawList 非空但池缺席"状态矛盾分支的互动）；`pending_rows`（实例槽缺/mesh 未驻留/材质缺席的暂缓行）在固定槽位命令 buffer 里如何表达（未驻留槽天然无命令 = 自动缺席，但诊断读数要对得上）——施工钉明。

## 6. 底座指向（不重复侦察）

- **Bevy 参照**（当前 checkout 实存，主计划 §5 已核实）：[build_indirect_params.wesl](../../../../crates/bevy_pbr/src/render/build_indirect_params.wesl)——indirect 命令组装的参照本体（标记→`instanceCount` 的写法、命令布局）；[mesh_preprocess.wesl](../../../../crates/bevy_pbr/src/render/mesh_preprocess.wesl) 与 [gpu_preprocess.rs](../../../../crates/bevy_pbr/src/render/gpu_preprocess.rs)、[batching/gpu_preprocessing.rs](../../../../crates/bevy_render/src/batching/gpu_preprocessing.rs)（两步 compute 的分工与 CPU 侧组织）。学数据依赖和取舍，不搬整套架构。
- **5.1/5.2 遗产**：[5.1 施工记录](../5.1-常驻绘制数据/5.1.1-5.1.6-常驻绘制数据施工记录：对象表、模型环与push退役.md)（对象记录 48B 三引脚布局断言）、[5.2 施工记录](../5.2-GPU视锥剔除/5.2.1-5.2.5-GPU视锥剔除施工记录：compute管线、可见标记与验证通道.md)（标记环/预置屏障/cull-stats）与[5.2 流程导读](../5.2-GPU视锥剔除/GPU视锥剔除流程：从相机平面到标记环的各级账面.md)（每帧剔除链——本段消费入口在链尾）。
- **时序契约**：命令 buffer 生命周期照标记环先例（compute 每帧全量重写、非上传链产物，双槽 + `wait_for_slot` fence 纪律，不新增等待闸门）；拆 Device 排空照既有三条销毁路径先例。
- **既有底座复用**：`DrawAssembly` 账本走访、BRP cull-stats 方法族、3.7/4.7 overlay 面板、诊断性回读放行口径（5.2 待决 7 拍板）。
