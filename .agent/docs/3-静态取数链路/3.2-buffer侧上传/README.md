# 施工 3.2：buffer 侧上传——顶点/索引进 GPU 大池

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。本目录存放本段任务与施工记录。

**目的**：将 `Assets<Mesh>` 数据去重送入 GPU 大池，验证合批、transfer 提交、跨队列依赖和资源安全复用。

**状态：🚧 施工中；3.2.1 已有实现，但 timeline 显式启用回归未关闭。**3.2.2～3.2.4 尚未实现。新增 A/B 探针的驱动运行记录已保留，但不能代替有效用法验证；本轮仅修文档。

## 前置闸门

- [ ] 按[《已实现缺陷与修复验收》](../材料/已实现缺陷与修复验收.md)修复 timeline 启用、提前 reset fence、present 信号量复用、退出等待时机；分别留下复测证据。
- [ ] 实际启用验证层与同步验证，确认日志/工具显示已启用；缺环境不得用“无告警”通过。
- [ ] 冻结上传完成、图形最后使用完成、staging/命令缓冲复用与退出排空的责任边界。

## 任务清单

- [ ] **3.2.1 Context 扩展（已实现部分待恢复验收）**：transfer 族选择与 Vulkan 1.3 硬校验已落地；保留 1.3 基线，恢复 `Vulkan12Features.timeline_semaphore(true)` 显式启用。`Vulkan12Features` 不等于兼容 1.2 设备。以验证层开启下的 timeline 创建及 signal/wait 验证，不只看 `timelineSemaphore=on` 或探针 PASS。
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
- timeline 探针：`ash_renderer/examples/timeline_probe.rs`，从仓库根可用 `cargo run -p ash_renderer --example timeline_probe` 运行。**现有探针未启用验证层且两组均未启用 synchronization2，不能据 PASS 宣称规范合规；待按缺陷文档修正后作为验收工具。**
- [已实现缺陷与修复验收](../材料/已实现缺陷与修复验收.md)：本段前置的存量代码与探针问题，不与尚未开发的上传功能混记。

## 待决问题

- staging 用整批暂存还是可复用环形区域：按本段最小可验证实现决定，必须能用完成票据证明安全。
- 静态容量增长可采用显式等待后的维护路径；频繁增量增长和流送预算留步骤 4～6，不纳入“正常上传零 idle”的承诺。
- 专用 transfer 队列是所选学习路径，其性能收益需实测，不因查到独立族就宣称复制必与图形硬件并行。
