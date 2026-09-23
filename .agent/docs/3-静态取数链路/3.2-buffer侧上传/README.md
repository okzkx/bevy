# 施工 3.2：buffer 侧上传——顶点/索引进 GPU 大池

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。本目录存放本段任务与施工记录。

**目的**：将 `Assets<Mesh>` 数据去重送入 GPU 大池，验证合批、transfer 提交、跨队列依赖和资源安全复用。

**状态：🚧 施工中；前置闸门四项已全部关闭（2026-09-23，见[缺陷文档 §9/§10](已实现缺陷与修复验收.md)），3.2.1 已验收，3.2.2～3.2.4 尚未实现。**

## 前置闸门

- [x] 按[《已实现缺陷与修复验收》](已实现缺陷与修复验收.md)修复 timeline 启用、提前 reset fence、present 信号量复用、退出等待时机；分别留下复测证据。（2026-09-23，feat `2cf35a2ff` + 执法轮 `4792aaa69` 修复 D1~D5 与新实抓的 D6；逐条记录见缺陷文档 §9/§10）
- [x] 实际启用验证层与同步验证，确认日志/工具显示已启用；缺环境不得用“无告警”通过。（2026-09-23：SDK 1.4.357.0 + `VkValidationFeaturesEXT` 常开，覆盖矩阵全绿零 VUID）
- [ ] 冻结上传完成、图形最后使用完成、staging/命令缓冲复用与退出排空的责任边界。

## 任务清单

- [x] **3.2.1 Context 扩展（已验收 2026-09-23）**：transfer 族选择与 Vulkan 1.3 硬校验落地；`Vulkan12Features.timeline_semaphore(true)` 已恢复显式启用（features2 查询 + 硬校验 + 日志报"支持→已启用"）。验证层开启下 timeline 创建/signal/wait 全链通过、负例被 VUID-03252 精准收账（缺陷文档 §10），不只看 `timelineSemaphore=on` 或探针 PASS。
- [ ] **3.2.2 资源模块**：在 `vulkan/` 内落位 `resources.rs` 或职责等价子模块，main 不加 Vulkan 细节。
  - [ ] **3.2.2.1 内存契约**：memory type、usage、绑定要求与对齐；staging 为 HOST_VISIBLE，是否 HOST_COHERENT 明确，必要时 flush 按 `nonCoherentAtomSize` 对齐。
  - [ ] **3.2.2.2 池与缓存**：顶点/索引 DEVICE_LOCAL 池 + bump 偏移；按资产身份去重缓存；初次容量按收齐的静态需求加余量分配，不因每帧快照重复分配。
  - [ ] **3.2.2.3 容量和销毁**：不足时走明确的维护/扩容路径，不写越界；等待旧使用完成后迁移/销毁，记录维护停顿。正常上传与维护分账。
- [ ] **3.2.3 顶点与索引转换**：POSITION/NORMAL/UV_0 → 选择的交错 32B vertex-input 布局；正确处理 U16/U32、字节偏移、对齐和 draw 的 firstIndex/vertexOffset；验证缺字段/不支持拓扑策略。
- [ ] **3.2.4 `flush_uploads`**：一次合批对应一个 staging 范围和一次 transfer 提交，不要求每批新建 staging 对象。
  - [ ] **3.2.4.1 提交与票据**：transfer 命令池归正确队列族；成功提交后记录单调 upload ticket；空批次不提交，失败不发布虚假完成票据。
  - [ ] **3.2.4.2 依赖**：EXCLUSIVE 跨族资源成对 release/acquire；同族无 ownership 转移但仍满足内存依赖。图形消费 vertex/index 数据的等待和屏障与实际访问匹配。
  - [ ] **3.2.4.3 复用**：staging 与 transfer command buffer 等上传完成后复用；目标范围和旧池等最后图形使用完成再覆盖/回收。
  - [ ] **3.2.4.4 失败和退出**：提交、录制或 acquire 失败不得留下永远无人 signal 的 fence/timeline 依赖；退出先排空所需 GPU 工作再拆资源。

使用 `vkQueueSubmit2` / barrier2 时须查询并显式启用 `synchronization2`；也可保留旧 SubmitInfo 加 TimelineSemaphoreSubmitInfo。接口选择在本段定案，不能混用未启用能力。

## 验证

1. 转换与上传字节数和 Assets 统计一致；抽查顶点/索引内容与偏移，不只对总字节数。
2. RenderDoc 等工具确认池数据就位；异步到货、重复快照不造成同一静态资产重复上传。
3. 正常上传路径无逐资源/逐批 `device_wait_idle` / `queue_wait_idle`；明确记录 fence/timeline 复用等待，容量维护另列。
4. 真正使用 timeline 并覆盖同族、异族的依赖分支；无法在本机运行的回退路径注明未实测。
5. 验证层和同步验证实际启用且目标路径零 WARNING/ERROR；resize、最小化、异常返回和退出回归通过。

## 材料清单

- [Timeline信号量：用法、与阻塞的区别、现代引擎的做法.md](Timeline信号量：用法、与阻塞的区别、现代引擎的做法.md)：机制篇（2026-09-23）——用法与启用前提（支持/启用/使用三件事）、与阻塞收口的对照、D3D12/UE/Unity 同位置做法；3.2.4 设计依据。
- [3.2.1-Context扩展：transfer队列族与timeline信号量开关.md](3.2.1-Context扩展：transfer队列族与timeline信号量开关.md)：队列选择、1.3 基线、支持与启用的勘误；保留历史实跑与新增探针的证据边界。
- timeline 探针 v2：`ash_renderer/examples/timeline_probe.rs`，从仓库根可用 `cargo run -p ash_renderer --example timeline_probe` 运行。**已修正（2026-09-23）**：请求验证层 + 同步验证 + synchronization2/timeline 显式启用，组 A（合法观察组）零 VUID、组 B（负例诊断组）被 VUID-03252 精准收账——可作为验收工具；旧 A/B 运行记录仅作历史观察保留。
- [验证层：CPU侧的规范执法者——它查什么、怎么查、保证到哪.md](验证层：CPU侧的规范执法者——它查什么、怎么查、保证到哪.md)：机制篇（2026-09-23）——驱动按契约假设你合法、layer 链与三个开关、四类执法面 + GPU-AV、保证的四条边界、"合同模拟执行者 vs GPU 监工"的复述校准。
- [已实现缺陷与修复验收](已实现缺陷与修复验收.md)：本段前置的存量代码与探针问题（D1~D6 + V1），不与尚未开发的上传功能混记。

## 待决问题

- staging 用整批暂存还是可复用环形区域：按本段最小可验证实现决定，必须能用完成票据证明安全。
- 静态容量增长可采用显式等待后的维护路径；频繁增量增长和流送预算留步骤 4～6，不纳入“正常上传零 idle”的承诺。
- 专用 transfer 队列是所选学习路径，其性能收益需实测，不因查到独立族就宣称复制必与图形硬件并行。
