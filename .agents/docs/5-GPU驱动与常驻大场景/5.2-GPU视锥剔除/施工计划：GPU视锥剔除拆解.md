# 施工计划：GPU视锥剔除拆解（5.2）

> 2026-10-15 立案（用户指令，纯文档立案）。
>
> **一句话说明**：这一段让 GPU 自己判断谁该画——compute 每帧对常驻对象表逐实例做"世界包围盒 vs 视锥六平面"求交、写出可见标记，给 5.3 indirect 铺路。本段只验"标记算得对"，不改画面、不做规模压测。对应[路线图](../../学习目标实现步骤.md)步骤 5 第二段与[步骤 5 施工计划](../施工计划：最小GPU驱动闭环与常驻大场景五段拆解.md) §5.2 节：compute 对常驻对象表做视锥→可见标记，GPU-driven 闭环的第二环——数据常驻（5.1）之后让 GPU 自己决定"谁在视锥里"，indirect（5.3）消费这份标记。本文管 5.2 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。段任务面板随开工转[段 README](README.md)。
>
> 场景口径沿用步骤定案：本段以 FlightHelmet 小场景验正确性（spawn 少量对象铺视锥内外样本），不做规模压测（归 5.5）。

## 0. 判定线（验收草案）

1. **可见标记与 CPU 参照一致**：GPU compute 算出的可见标记与 CPU 参照计算（复用 5.1 的 `conservative_world_aabb` + 平面求交）逐对象一致——BRP 驱动移动对象出入视锥、spawn 视锥内外分布样本、非均匀缩放 + 旋转边界样本；**保守语义 = 相交即可见，不误剔**（宁多画不错杀）。CPU 参照只做正确性验证，不驱动绘制。
2. **红线**：CPU 每对象可见性剔除不存在；可见集不回读驱动绘制（与 5.3 共同遵守，主计划 §2 边界原文）；诊断性计数回读**已放行**（2026-10-15 用户拍板，§5 待决 7）——只进计量（egui 面板/BRP 读数），任何系统不得读它决定画什么，读数通道单独标注。
3. **compute 写→绘制读的依赖显式化**：dispatch 与后续图形读取之间有显式屏障，同步验证层零误报（同步验证层对 device 侧 compute→graphics 依赖有效——与它对 host 写全盲不同，本判定线可执法）。
4. **前置闸门四件留痕**（§2）：队列族 compute 支持实测读数、compute 编译链过 build.rs 全链、spirv-val 过、compute pipeline 空转首跑零 VUID——各留证据后才开本段主体。
5. **既有判定线零回退**：4.x 增量链全量（稳态零上传零盘点、移动恰一行、槽位闭环）、3.5 三模式截图零回退（**本段标记不进绘制路径，画面必须与 5.1 基线逐像素一致**）、零 VUID（验证层覆盖 compute 命令）、WM_CLOSE exit 0、clippy 全净、resize/最小化与 compute 资源生命周期共存（主计划 §3 新增回归项）、拆 Device 排空不炸。

**对主计划判定线③的一处立案期辨析（落点已拍板）**：主计划 5.2 判定线③"剔除开关 A/B 画面一致（只被剔除的都不可见）"需要**绘制侧消费可见标记**才成立——本段红线（不回读驱动绘制）封死了 CPU DrawList 过滤这条路，而 indirect 消费归 5.3。落点已定（2026-10-15 用户拍板）：标记**数据层** A/B 对照放 5.2、画面级 A/B 顺延 5.3 判定线——无脚手架代码，责任边界清楚：5.2 管"标记对不对"，5.3 管"消费得对不对"；主计划判定线③的画面级部分随此挪至 5.3。

## 1. 定位：现状与要换掉的东西

5.1 收官后的绘制链：三表常驻 GPU（对象表 131072 槽 × 48B 记录含**局部 AABB**、模型环本帧槽、材质环），shader 按实例索引读 set2，DrawList = CPU 槽位枚举（O(N)），逐对象照画——**可见性 = 全画，无剔除**。5.2 把"谁在视锥里"也交给 GPU：每帧 compute dispatch 对在场槽位做世界 AABB 变换 + 视锥求交，写一份可见标记；CPU 组装侧不变、不感知标记（消费归 5.3）。

**5.1 遗产中 compute 侧已就位的部分**（本段直接受益，不重复施工）：

- set2 三条只读 storage 的 descriptor 布局声明 `VERTEX|FRAGMENT|COMPUTE` 超集（`vulkan/instance_table.rs:21`）——compute 读对象表/模型环**零 descriptor 改动**；
- 对象记录带局部 AABB（世界推导消费端原定归本段）——非均匀缩放保守化的数据源头已入库；
- CPU 参照现成：`conservative_world_aabb`（8 角点重紧 + 4 个单测，`instance_table.rs:155`）——判定线 1 的参照计算直接复用；
- `BufferRole::DeviceStorage`、帧槽环惯例（帧 UBO/模型环 ×2 + `wait_for_slot` fence 纪律）——标记 buffer 落位可照抄形状。

**立案期核实订正一处主计划失真**：主计划 §5.2 写"barrier 形状，synchronization2 已启用"——**不属实**。现状：设备 feature 启用面 = timelineSemaphore、dynamicRendering、descriptorIndexing+4 位、swapchain_mutable_format（`vulkan/context.rs:410` 就绪日志所列，无 synchronization2）；全部现役屏障走传统 `cmd_pipeline_barrier`（`vulkan/uploader.rs` 跨族所有权、`vulkan/frames.rs` 渲染路径多处）。instance 级启用的 `SYNCHRONIZATION_VALIDATION` 是**同步验证层**（诊断工具，`context.rs:143`），与 synchronization2（设备 feature，屏障 API v2 形状）是两件事。本段屏障按传统形状起步即可（buffer memory barrier 覆盖 compute 写→图形读），synchronization2 是否启用为待决（§5 待决 1）。

## 2. 前置闸门：compute 环境四件（最先做，过闸才开主体）

主计划已立"支持≠验证"闸门——compute 是 1.0 core 无 feature 开关，但查询必 true ≠ 用法合规，驱动接受 ≠ 合法（VUID 只有验证层执法，"跑通"当合规证明是方法论错误）。四件按序：

| # | 闸门 | 现状核实（立案时点） | 实测动作 |
|---|---|---|---|
| 1 | 图形队列族 compute 支持确认 | 选族只过滤 `GRAPHICS` 位（`context.rs:216-236`），未显式查 COMPUTE、未建 compute 队列；transfer 专族选择已有先例可循 | `get_physical_device_queue_family_properties` 打印各族 flags 留痕；桌面独显图形族几乎必含 COMPUTE，实测钉明；**图形族含 compute → 同族同队列最简**（barrier 即可，无跨队列信号量）；备选独立 compute 族 = 跨族所有权 + 队列间同步（uploader 已有跨族 barrier 先例，但队列间提交同步没有） |
| 2 | naga compute WGSL→SPIR-V 编译链 | build.rs（naga 30.0.1）按模块整体编译、**无 stage 过滤**（`build.rs:68-91`），现役 shader 只有 vertex/fragment 入口，compute 链未实测 | 写最小 `.wgsl`（`@compute @workgroup_size(64)` 空转 main）过 build.rs 全链：wgsl 解析 → naga validate（capability 面与补丁器路径对 compute 模块的适用性）→ spv-out（GLCompute 入口发射）→ spirv-val `--target-env vulkan1.3` |
| 3 | spirv-val 过 | 闸门已通用（模块级校验，不分 stage） | 随 2 一并过，独立留痕 |
| 4 | compute pipeline 空转首跑 | `vkCreateComputePipelines` 无动态渲染/附件声明纠缠，比图形管线简单 | 建 pipeline + 绑 set2 现有帧描述符 + dispatch 空转 + 依赖屏障，验证层零 VUID |

闸门产物（队族 flags 读数、编译日志、空转首跑证据）直接进 5.2 施工记录，不另立文档。

## 3. 任务拆解草案（开工时转段 README 任务面板）

| 任务 | 内容 | 验收锚点 |
|---|---|---|
| 5.2.1 前置闸门四件 | §2 全表 | 判定线 4 留痕 |
| 5.2.2 可见标记与视锥数据 | 标记 buffer（`DeviceStorage`，**2 帧槽环**——与帧 UBO/模型环同链：compute 写与 draw 读同命令缓冲时同槽两次使用隔 `wait_for_slot` fence，无新等待闸门）；视锥 6 平面来源（set1 UBO 扩容 vs 新 binding，§5 待决 2；倾向 CPU 算平面随帧 UBO 走）；compute pipeline 布局（set2 只读复用 + 标记 buffer 的 binding 落位，§5 待决 3） | 数据结构定案；标记槽与帧槽对齐不串帧 |
| 5.2.3 compute shader 与 dispatch | 世界 AABB 变换（8 角点 vs 列最大缩放，§5 待决 4；倾向与 CPU 参照同款 8 角点，保证判定线 1 逐位可比）+ 6 平面求交（相交即可见）+ 写标记；dispatch 范围 = `used_objects` 上界取整到 workgroup（全容量 131072 dispatch 浪费 99.9%）；dispatch 记录位置 = 图形命令缓冲内（同族同队列，§5 待决 6） | 判定线 1/3 可测 |
| 5.2.4 验证通道与开关 | CPU 参照对照（BRP 驱动样本集）；诊断性计数回读接线（已放行，§5 待决 7——只进计量、单独标注）；A/B 开关落标记数据层（已拍板，§5 待决 5）；egui 计量挂现有 overlay | 判定线 1/2 证据成形 |
| 5.2.5 验收回归 | 判定线全量 + 负例（裁 Device、resize、despawn 后槽位标记不串）+ 回归矩阵 | §0 全过 |

顺序理由：闸门先过（compute 环境不成立则主体全空谈），数据结构次之（标记 buffer 与视锥入口定形状），shader 与 dispatch 再后（有表可读、有标记可写），验证通道最后（有标记可对照）。5.2.2~5.2.3 是本段主体；5.2.4 只做验证不铺新数据路径。

**与冻结边界的关系**：本段不动对象表/材质表记录布局（5.1 已定）；可能动 set1 UBO 布局（视锥平面进 UBO 的话，§5 待决 2）——UBO 128B 冻结在 5.1 已解冻过一次，解冻范围清单照 5.1.1 先例随 5.2.2 落。标记 buffer 是新增 binding，落位选择不影响 set2 只读三表的既有声明（descriptor 写侧追加）。

## 4. 测试方案与证据形态

- **BRP 驱动**（既有方法族全在库）：set_transform 移动对象出入视锥（标记翻转逐对象对照）、spawn 环形分布铺视锥内外样本、replace 换 mesh（AABB 随记录换、标记随行变）；brp.py 子命令随需扩（如可见计数读数——以待决 7 放行为前提）。
- **CPU 参照专项**：参考实现 = `conservative_world_aabb`（库内现成）+ 6 平面点积求交；样本覆盖：视锥内/外/跨平面/非均匀缩放 + 旋转四类，BRP 布置、读数对照留档。
- **计量读数**：可见/不可见计数（若回读放行）、dispatch 组数、标记槽位复用读数；egui 挂 3.7/4.7 现有面板。
- **截图读图**（按[画面功能视觉验证](../../../rules/画面功能视觉验证.md)）：三模式零回退（本段画面不变是判定线 5 的一部分）；画面级 A/B 已拍板顺延 5.3，本段画面证据只验"与 5.1 基线一致"。
- **负例**：despawn 后槽位被复用——旧标记不得串到新对象（槽位与标记同界对照）；窗口 resize/最小化时 compute 资源与 swapchain 生命周期共存；拆 Device 排空（标记 buffer 进停尸房口径）。
- **回归矩阵**：4.x 全量判定线、3.5 三模式、零 VUID（验证层开启含 compute 命令覆盖）、WM_CLOSE exit 0、clippy 全净、增删后 resize×2 不炸。

## 5. 待决问题（施工时钉明，不写死）

1. **屏障 API 形状**：传统 `cmd_pipeline_barrier`（现状，buffer barrier 覆盖 compute→graphics，本段够用）vs 启用 synchronization2 feature（1.3 core，设备创建面 + VUID 面变化，全局迁移成本）。倾向传统先通；启用 sync2 的翻案条件 = 出现传统形状表达不了的依赖（如 timeline 信号量 2 形状），施工时评估。
2. **视锥平面来源**：CPU 算好 6 平面（Gribb-Hartmann）随帧 UBO 写入（每次 UBO 全量写多 96B，稳态成本可忽略；数学留在 CPU，shader 简单）vs GPU 从 view_proj 自提（省 96B，不值得）。倾向前者；连带核实 set1 UBO 128B 余量与扩容方式（解冻清单）。
3. **标记 buffer 组织与 binding 落位**：每对象 1 位（u32 位表，省 32 倍）vs 1 字（u32/槽，读写简单、槽位对齐天然）——倾向 1 字起步，1 位留给 5.4 规模化再评估；binding 落 set2 追加（写侧追加、shader 只读三表声明不动）vs 独立小布局——倾向 set2 追加（同为帧级数据、同批描述符）。
4. **AABB 保守化 GPU 形状**：8 角点重紧（与 CPU 参照 `conservative_world_aabb` 同款，逐位可对照，shader 24 次乘加）vs 列最大缩放（更便宜，保守性形状与参照不同、对照须另算容差）——倾向 8 角点，与判定线 1 的逐位一致要求配套。
5. **判定线③画面级 A/B 落点（已落定 2026-10-15 用户拍板）**：标记数据层 A/B 放 5.2、画面级 A/B 顺延 5.3——无脚手架、责任边界清楚（5.2 管"标记对不对"、5.3 管"消费得对不对"）；顶点级可见性剔除脚手架方案否决（一套 5.3 后退役的一次性代码不值得）。
6. **compute 提交形状**：图形命令缓冲内 dispatch（同族同队列，barrier 排序即可，无跨队列同步；倾向）vs 独立 command buffer 同队列提交（隔离清晰，多一次提交开销）vs 独立 compute 队列族（闸门 1 实测图形族不含 compute 时的兜底，跨族所有权 + 队列间信号量）。
7. **诊断性计数回读边界（已落定 2026-10-15 用户拍板）**：放行——每帧（或低频）回读一个小 u32 可见计数进 egui 面板/BRP 读数；边界 = 只进计量与观测通道，任何系统不得读它决定画什么（红线不变），读数通道在计量面板与 brp.py 读数处单独标注"诊断性回读，不驱动绘制"。

## 6. 底座指向（不重复侦察）

- **Bevy 参照**（当前 checkout 实存）：[mesh_preprocess.wesl](../../../../crates/bevy_pbr/src/render/mesh_preprocess.wesl)——compute 视锥剔除的参照本体（view_frustum 平面与 AABB 求交的 GPU 形状、workgroup 划分、输出写法）；[gpu_preprocess.rs](../../../../crates/bevy_pbr/src/render/gpu_preprocess.rs) 三路线的数据依赖。学求交与 dispatch 形状，不搬整套架构。
- **5.1 遗产**：[施工记录](../5.1-常驻绘制数据/5.1.1-5.1.6-常驻绘制数据施工记录：对象表、模型环与push退役.md)（对象记录布局/局部 AABB/set2 声明）、[环机制篇](../5.1-常驻绘制数据/环机制：HostVisible数据的帧槽轮转与fence纪律.md)（帧槽 + fence 纪律——标记 buffer 双槽照此链）。
- **时序契约**：标记 buffer 生命周期遵守[责任边界契约](../../3-静态取数链路/3.2-buffer侧上传/责任边界冻结：上传完成、图形消费与退出排空的时序契约.md)——标记每帧由 compute 全量重写（非上传链产物），双槽 + `wait_for_slot` 同链即可，不新增等待闸门；拆 Device 排空照既有三条销毁路径先例。
- **既有底座复用**：`conservative_world_aabb`（判定线 1 参照）、`BufferRole::DeviceStorage`、3.7/4.7 overlay 面板、BRP 方法族（4.1/4.3）。
