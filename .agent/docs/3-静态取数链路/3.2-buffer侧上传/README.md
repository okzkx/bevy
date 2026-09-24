# 施工 3.2：buffer 侧上传——顶点/索引进 GPU 大池

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。本目录存放本段任务与施工记录。

**目的**：将 `Assets<Mesh>` 数据去重送入 GPU 大池，验证合批、transfer 提交、跨队列依赖和资源安全复用。

**状态：✅ 已收官（2026-09-24）；前置闸门四项全部关闭，3.2.1～3.2.4 全部验收（探针 + App 实跑证据，见任务清单逐项标注；3.4 接线时须遵守[责任边界](责任边界冻结：上传完成、图形消费与退出排空的时序契约.md)冻结的形状）。**

## 前置闸门

- [x] 按[《已实现缺陷与修复验收》](已实现缺陷与修复验收.md)修复 timeline 启用、提前 reset fence、present 信号量复用、退出等待时机；分别留下复测证据。（2026-09-23，feat `2cf35a2ff` + 执法轮 `4792aaa69` 修复 D1~D5 与新实抓的 D6；逐条记录见缺陷文档 §9/§10）
- [x] 实际启用验证层与同步验证，确认日志/工具显示已启用；缺环境不得用“无告警”通过。（2026-09-23：SDK 1.4.357.0 + `VkValidationFeaturesEXT` 常开，覆盖矩阵全绿零 VUID）
- [x] 冻结上传完成、图形最后使用完成、staging/命令缓冲复用与退出排空的责任边界。（2026-09-24，[责任边界冻结：上传完成、图形消费与退出排空的时序契约.md](责任边界冻结：上传完成、图形消费与退出排空的时序契约.md)——四等待面各归其主 + 分账表；图形消费面冻结形状由探针组 D 预演，3.4 接线遵守）

## 任务清单

- [x] **3.2.1 Context 扩展（已验收 2026-09-23）**：transfer 族选择与 Vulkan 1.3 硬校验落地；`Vulkan12Features.timeline_semaphore(true)` 已恢复显式启用（features2 查询 + 硬校验 + 日志报"支持→已启用"）。验证层开启下 timeline 创建/signal/wait 全链通过、负例被 VUID-03252 精准收账（缺陷文档 §10），不只看 `timelineSemaphore=on` 或探针 PASS。
- [x] **3.2.2 资源模块（已验收 2026-09-24）**：在 `vulkan/` 内落位 `resources.rs` 或职责等价子模块，main 不加 Vulkan 细节。（终态 = 契约/池/上传/转换四子模块 + 编排 `upload.rs`；main 只 `add_plugins(AshUploadPlugin)`,零 Vulkan 符号——资源件与探针全部在 `vulkan/` 与 `examples/`）
  - [x] **3.2.2.1 内存契约（已验收 2026-09-24）**：memory type、usage、绑定要求与对齐；staging 为 HOST_VISIBLE，是否 HOST_COHERENT 明确，必要时 flush 按 `nonCoherentAtomSize` 对齐。（落位 `vulkan/resources.rs`：角色定案表 + `find_type` 必需/优先两级选型 + offset 0 绑定 + persistent map + write/read 内建 coherent 分支；探针 `ash_renderer/examples/memory_probe.rs` 组 A 零 VUID、拷贝闭环逐字节一致，组 B 负例被对齐条款收账；非 coherent flush 分支本机无该内存类型、未实测，记录见[施工记录 §3](3.2.2.1-内存契约：memory type、usage、绑定与对齐.md)）
  - [x] **3.2.2.2 池与缓存（已验收 2026-09-24）**：顶点/索引 DEVICE_LOCAL 池 + bump 偏移；按资产身份去重缓存；初次容量按收齐的静态需求加余量分配，不因每帧快照重复分配。（落位 `vulkan/pool.rs`：双池 bump + `AssetId<Mesh>` 驻留缓存 + 账本行携带 vertex_base/first_index/票据；App 实跑 6 资产单批进池、字节数与采集统计逐字节吻合、重复快照零重复上传；探针组 B 四段读回逐字节一致；记录见[施工记录](3.2.2.2-池与缓存：DEVICE_LOCAL大池、bump分配与资产身份去重.md)）
  - [x] **3.2.2.3 容量和销毁（已验收 2026-09-24）**：不足时走明确的维护/扩容路径，不写越界；等待旧使用完成后迁移/销毁，记录维护停顿。正常上传与维护分账。（探针组 C：1MiB 池被 1.2MiB 资产触发一次显式扩容，等在飞→整段迁移→等迁移票据→销毁旧池，迁移后内容逐字节复核一致，`MaintenanceLedger` 分账；App 全程零维护即分账为零；施工记录见[同文档 §3](3.2.2.3-容量与销毁：显式维护迁移与停顿分账.md)）
- [x] **3.2.3 顶点与索引转换（已验收 2026-09-24）**：POSITION/NORMAL/UV_0 → 选择的交错 32B vertex-input 布局；正确处理 U16/U32、字节偏移、对齐和 draw 的 firstIndex/vertexOffset；验证缺字段/不支持拓扑策略。（落位 `vulkan/mesh_convert.rs` 纯函数：offset 0/12/24 小端逐字节验证，U16→U32 统一加宽，缺字段 0 填充，拓扑/空/缺 POSITION 三种拒绝错误类型精确匹配；flightHelmet 55392 顶点全转换成功；探针组 A + [施工记录](3.2.3-顶点与索引转换：32B交错布局、索引统一与拒绝策略.md)）
- [x] **3.2.4 `flush_uploads`（已验收 2026-09-24）**：一次合批对应一个 staging 范围和一次 transfer 提交，不要求每批新建 staging 对象。（落位 `vulkan/uploader.rs` 机制 + `upload.rs` 系统：FlightHelmet 6 资产实跑即单批 #1；App 编排住 Last、先于帧循环；施工记录见[3.2.4 篇](3.2.4-flush_uploads：合批提交、timeline票据与跨族依赖.md)，接口定案 = 旧 SubmitInfo + TimelineSemaphoreSubmitInfo，不启用 synchronization2）
  - [x] **3.2.4.1 提交与票据**：transfer 命令池归正确队列族；成功提交后记录单调 upload ticket；空批次不提交，失败不发布虚假完成票据。（专用 transfer 族 1 本机实测；票据 #1/#2/#3 单调实录；空批与失败路径的结构保证见 [3.2.4 篇 §3.2.4.1](3.2.4-flush_uploads：合批提交、timeline票据与跨族依赖.md)）
  - [x] **3.2.4.2 依赖**：EXCLUSIVE 跨族资源成对 release/acquire；同族无 ownership 转移但仍满足内存依赖。图形消费 vertex/index 数据的等待和屏障与实际访问匹配。（探针组 D 在专用 transfer 族上全链实跑：上传批末 release → graphics 提交等票据 @ALL_COMMANDS + acquire + 真实读，逐字节一致、CPU 不阻塞；同族回退路径本机无硬件载体注明未实测，形状由 memory_probe 同族闭环覆盖）
  - [x] **3.2.4.3 复用**：staging 与 transfer command buffer 等上传完成后复用；目标范围和旧池等最后图形使用完成再覆盖/回收。（2 槽轮转 + 槽位票据闸门，每次等待有日志；池区间 bump 只增不覆盖，覆盖场景归维护等待纪律；槽位复用实际阻塞分支本机未观测到，注明未实测）
  - [x] **3.2.4.4 失败和退出**：提交、录制或 acquire 失败不得留下永远无人 signal 的 fence/timeline 依赖；退出先排空所需 GPU 工作再拆资源。（submit_batch 早退先于发票据的结构保证；退出排空仍归 teardown 的 device_wait_idle 单点，D4 边界不变）

使用 `vkQueueSubmit2` / barrier2 时须查询并显式启用 `synchronization2`；也可保留旧 SubmitInfo 加 TimelineSemaphoreSubmitInfo。接口选择在本段定案，不能混用未启用能力。

## 验证

1. ✅ 转换与上传字节数和 Assets 统计一致（App：顶点 1772544B = 55392×32、索引 1136664B = 284166×4）；抽查顶点/索引内容与偏移由探针读回逐字节承担（组 B 四段、组 C 迁移后四段），不只对总字节数。
2. ✅ 池数据就位以读回校验为证据（探针逐字节比对；RenderDoc 人工可查,不作验收闸门）；重复快照零重复上传由驻留缓存 + 票据停走证明（App『静态资产全部驻留』后零新增批次）。
3. ✅ 正常上传路径无逐资源/逐批 `device_wait_idle` / `queue_wait_idle`（App 全程零 idle 调用）；fence/timeline 复用等待有日志且归复用/维护/退出/消费四类合法调用点（见[责任边界分账表](责任边界冻结：上传完成、图形消费与退出排空的时序契约.md)），容量维护另列（组 C 实测一次分账）。
4. ✅ 真正使用 timeline 并覆盖同族、异族的依赖分支：signal（探针/App）、graphics 侧 wait（探针组 D）、CPU wait_semaphores（组 B/C）全链实测；异族 release/acquire 在本机专用 transfer 族（族 1）实跑,同族回退路径本机无硬件载体,注明未实测（形状由 memory_probe 同族闭环覆盖）。
5. ✅ 验证层和同步验证实际启用（SDK 1.4.357.0 + VkValidationFeaturesEXT 常开）且目标路径零 WARNING/ERROR（探针四组 + App 上传/清屏/退出全程）；resize、最小化、异常返回和退出回归由帧循环既有闸门承担（V1 覆盖矩阵 + 缺陷文档 D1~D6）。

## 材料清单

- [Timeline信号量：用法、与阻塞的区别、现代引擎的做法.md](Timeline信号量：用法、与阻塞的区别、现代引擎的做法.md)：机制篇（2026-09-23）——用法与启用前提（支持/启用/使用三件事）、与阻塞收口的对照、D3D12/UE/Unity 同位置做法；3.2.4 设计依据。
- [3.2.1-Context扩展：transfer队列族与timeline信号量开关.md](3.2.1-Context扩展：transfer队列族与timeline信号量开关.md)：队列选择、1.3 基线、支持与启用的勘误；保留历史实跑与新增探针的证据边界。
- timeline 探针 v2：`ash_renderer/examples/timeline_probe.rs`，从仓库根可用 `cargo run -p ash_renderer --example timeline_probe` 运行。**已修正（2026-09-23）**：请求验证层 + 同步验证 + synchronization2/timeline 显式启用，组 A（合法观察组）零 VUID、组 B（负例诊断组）被 VUID-03252 精准收账——可作为验收工具；旧 A/B 运行记录仅作历史观察保留。
- [验证层：CPU侧的规范执法者——它查什么、怎么查、保证到哪.md](验证层：CPU侧的规范执法者——它查什么、怎么查、保证到哪.md)：机制篇（2026-09-23）——驱动按契约假设你合法、layer 链与三个开关、四类执法面 + GPU-AV、保证的四条边界、"合同模拟执行者 vs GPU 监工"的复述校准。
- [3.2.2.1-内存契约：memory type、usage、绑定与对齐.md](3.2.2.1-内存契约：memory type、usage、绑定与对齐.md)：施工记录（2026-09-24）——角色定案表、绑定四条款、coherent 分支与 atom 舍入三细则、探针证据与未实测边界；`vulkan/resources.rs` 与 memory_probe 的设计依据。
- [GpuBuffer：契约、用法与frenderer三分法对照.md](GpuBuffer：契约、用法与frenderer三分法对照.md)：设计对照篇（2026-09-24）——GpuBuffer 定位与用法速查、与 frenderer RO/RW/MRW 的轴对照（角色表达用法 + 运行时同步语义 + 类型层留白）、Bindless 对 buffer 的"稳定与寿命"要求、宿主可见内存的两码头角色与 3.5 每帧 UBO 展望。
- 内存契约探针：`ash_renderer/examples/memory_probe.rs`，从仓库根可用 `cargo run -p ash_renderer --example memory_probe` 运行。组 A（合同正路）在验证层 + 同步验证下零 VUID，含 staging→池→回读的最小 copyBuffer 闭环；组 B（绑定负例）被 VUID-10739（即 v1.3.289 的 memoryOffset-01036）收账。
- [3.2.2.2-池与缓存：DEVICE_LOCAL大池、bump分配与资产身份去重.md](3.2.2.2-池与缓存：DEVICE_LOCAL大池、bump分配与资产身份去重.md)：施工记录（2026-09-24）——双池 bump、身份去重、账本行引脚与容量策略；`vulkan/pool.rs` 设计依据。
- [3.2.2.3-容量与销毁：显式维护迁移与停顿分账.md](3.2.2.3-容量与销毁：显式维护迁移与停顿分账.md)：施工记录（2026-09-24）——维护五步时序、停顿分账、图形侧等待面冻结；组 C 证据。
- [3.2.3-顶点与索引转换：32B交错布局、索引统一与拒绝策略.md](3.2.3-顶点与索引转换：32B交错布局、索引统一与拒绝策略.md)：施工记录（2026-09-24）——布局契约、索引统一 U32、拒绝策略；组 A 证据。
- [3.2.4-flush_uploads：合批提交、timeline票据与跨族依赖.md](3.2.4-flush_uploads：合批提交、timeline票据与跨族依赖.md)：施工记录（2026-09-24）——四子项定案、合批形状、失败/退出结构保证、施工插曲（staging 漏写）。
- [责任边界冻结：上传完成、图形消费与退出排空的时序契约.md](责任边界冻结：上传完成、图形消费与退出排空的时序契约.md)：前置闸门③（2026-09-24）——四等待面归主 + 分账表，3.4 接线契约。
- 上传链探针：`ash_renderer/examples/upload_probe.rs`，从仓库根可用 `cargo run -p ash_renderer --example upload_probe` 运行。组 A（转换逐字节 + 拒绝精确）、组 B（上传闭环 + 票据 + 去重）、组 C（维护迁移内容保持 + 分账）、组 D（跨族 release/acquire + graphics 队列真实读）在验证层 + 同步验证下全程零 VUID（2026-09-24 实跑记录见 3.2.4 篇 §3）。
- [已实现缺陷与修复验收](已实现缺陷与修复验收.md)：本段前置的存量代码与探针问题（D1~D6 + V1），不与尚未开发的上传功能混记。

## 待决问题

- ~~staging 用整批暂存还是可复用环形区域~~：已定案（2026-09-24）——2 槽环形轮转复用，槽位票据闸门证明复用安全（3.2.4.3）；『整批一个 staging 范围』与『不逐批新建对象』由两槽按需扩容统一满足。
- ~~静态容量增长可采用显式等待后的维护路径~~：已定案（2026-09-24）——维护路径落地（3.2.2.3 五步时序 + 停顿分账）；频繁增量增长和流送预算仍留步骤 4～6，不纳入『正常上传零 idle』的承诺。
- 专用 transfer 队列是所选学习路径，其性能收益需实测，不因查到独立族就宣称复制必与图形硬件并行。
