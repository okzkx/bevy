# Timeline信号量：用法、与阻塞的区别、现代引擎的做法

3.2 上传链的机制篇之一，对应[施工 3.2](README.md)与[3.2.1 记录](3.2.1-Context扩展：transfer队列族与timeline信号量开关.md)。回答两个问题：**timeline 信号量怎么用、它与 CPU 阻塞式收口的本质区别**；**现代引擎（D3D12/UE/Unity）在同一位置怎么做事**。本篇是 3.2.4 `flush_uploads` 设计依据，机制口径已按 2026-09-23 校验勘误修订——"支持、启用、使用是三件事"，显式启用是定案，探针只作驱动行为观察。

## §0 一句话

timeline 信号量 = 队列上可反复推进、CPU/GPU 双方可读的**单调进度值**：上传批每完成一批把值推进一档，渲染提交在 GPU 侧"等到某值才开始"，CPU 随时可查进度——它把"等拷贝完成"从 CPU 阻塞（frenderer 式 `device_wait_idle`）搬进 GPU 依赖链，这正是 D3D12 fence value 流水在 Vulkan 的对应物。

## §1 用法

### binary 与 timeline 的对照

| | binary（现有帧循环在用） | timeline（3.2.4 引入） |
|---|---|---|
| 状态 | 响/未响，两态 | 64 位单调递增值，一次创建可推进无限次 |
| 提交接口 | `SubmitInfo` 的 wait/signal（不带值） | `SubmitInfo2` + `SemaphoreSubmitInfo::value` |
| host 等待 | 不能等（只能 fence） | `vkWaitSemaphores`（带超时等到某值） |
| host 查询 | 无值可查 | `vkGetSemaphoreCounterValue` 随时查 |
| 复用 | 用完 reset，signal/wait 严格一一配对 | 无需 reset；signal N+1 不要求 N 已被消费 |
| 典型岗位 | present 握手（image_available/render_finished） | 上传批次完成票据 |

创建：在 `SemaphoreCreateInfo` 的 pNext 链上挂 `SemaphoreTypeCreateInfo { semaphore_type: TIMELINE, initial_value }`。

### 启用前提：支持、启用、使用是三件事

这是 3.2.1 校验勘误的核心结论，写机制篇必须先立住：

1. **支持**：`vkGetPhysicalDeviceFeatures2` 查得 `Vulkan12Features.timelineSemaphore = true`，只说明设备*可以*被启用成能用 timeline。
2. **启用**：逻辑设备创建时必须链 `Vulkan12Features.timeline_semaphore(true)`——`VUID-VkSemaphoreTypeCreateInfo-timelineSemaphore-03252` 要求创建 TIMELINE 型信号量时该 feature 已启用；同理，用 `vkQueueSubmit2` 必须启用 `synchronization2`（`VUID-vkQueueSubmit2-synchronization2-03866`）。
3. **使用**：以上两条齐备后的提交/等待才合法。

三个必须纠正的旧读法（详见 3.2.1 记录 §3）：

- **`Vulkan12Features` 的名字不是"兼容 1.2 设备"**。它收纳的是"晋升进 1.2 核心的细粒度 feature"，1.3 应用照用它声明 timeline 等能力；`Vulkan13Features` 只收本批新聚合的 feature，没有再次列 timeline，不等于启用约束消失。
- **1.2 起支持即 mandatory，但"必报 true"从不等于"默认启用"**。Vulkan 的 feature 模型里，查询值为 true 只是启用资格，用不用得上取决于 `vkCreateDevice` 时的声明。
- **驱动接受 ≠ 合法**。VUID 是 valid usage，实现不被要求在运行时逐条检查，违反 VUID 的行为是未定义的；VUID 的执法者是验证层，不是驱动。此前探针组 A"不链任何 features 仍全链 PASS"只能证明 NVIDIA 驱动没做这条运行时检查，不能证明删链合法——何况探针两组都用了 `queue_submit2` 却未启用 `synchronization2`，本身就踩着另一条 VUID。

启用位的成本是零（一个结构、一个 bool），换来的是：依赖在设备创建处可见；验证层能在"未启用却使用"的真实误用时当场报警；3.3 的 descriptor-indexing 各位（同样收在 `Vulkan12Features`）沿用同一姿势。

### 本工程 3.2.4 的预定用法

按[施工计划 §3](../施工计划：Bindless起步五段拆解.md)细化后的 3.2.4 子项：

1. **票据**：一次合批 = 一个 staging 范围 + 一次 transfer 提交；提交成功后把 timeline 推进到单调递增的 upload ticket（如 10、20、30）。空批不提交，失败不发虚假票据（3.2.4.1）。
2. **依赖**：渲染提交（graphics 队列）wait 票据值，dst stage = `VERTEX_INPUT`——顶点着色器真读数据之前收口；CPU 只配好依赖即返回，不阻塞。跨族 EXCLUSIVE 资源成对 release/acquire，同族免所有权转移但内存依赖照表达（3.2.4.2）。
3. **回收**：staging 与 transfer 命令缓冲等"上传完成"（ticket 被消费）后复用；被覆盖的目标范围等"最后图形使用完成"再回收（3.2.4.3）。CPU 侧用 `vkGetSemaphoreCounterValue` 查进度决定可复用窗口，或 `vkWaitSemaphores` 精确等到某值。
4. **退出**：拆除前等待在飞工作完成再销毁资源——这是 CPU 阻塞等待的正确用武之地，与"正常路径不阻塞"不矛盾（3.2.4.4）。

## §2 与阻塞的区别

### 两种收口

frenderer 的形状：每 primitive 提交拷贝 → `device_wait_idle` → 等全设备空闲 → 继续下一步，一帧内如此两次。**等待发生在 CPU**：GPU 与 CPU 串行，同步原语的需求被"原地阻塞"覆盖，binary 信号量只管 present 握手。

本工程 3.2 的形状：flush 只做"把依赖配好"的动作（渲染提交携带 wait 票据值）就返回；**等待发生在 GPU**——渲染命令在队列上等值，等到才开始顶点读取。CPU 提交线程从不为上传停车。

| | 阻塞收口（frenderer） | GPU 侧依赖（本工程 3.2.4） |
|---|---|---|
| 等待位置 | CPU 原地阻塞 | GPU 队列内 wait value |
| 上传与渲染 | 串行，互等 | 并行在飞，按依赖收口 |
| 等待粒度 | 全设备（wait_idle） | 精确到"第 N 批数据" |
| 判定线 | 每 primitive 两次 wait_idle（被超越的对象） | 正常路径零逐资源/逐批 idle |

### 为什么 binary / fence 不够

- **binary 表达不了进度**：连续 signal 未被消费的同一 binary 违反一一配对约束；上传是"帧 N、N+1、N+2 的批连续在飞"的常态，需要的是 10 → 20 → 30 的单调值。渲染侧"等 20"一次覆盖前两批，不必枚举对象。
- **fence 表达不了进度、也没有提交间依赖**：fence 是 CPU 收割单次提交完成的工具（每批一个对象），挂不上"渲染命令在 GPU 里等某批"的语义。
- **同族退回也免不了显式依赖**：同一队列上两次提交保证执行顺序，但跨提交的内存可见性不自动成立——拷贝提交与渲染提交之间照样要 wait/signal，只是等待的形式可以更便宜。这就是 3.2.4.2"同族无 ownership 转移但仍满足内存依赖"的含义。

### 阻塞并没有被全盘否定

等待口径（施工计划 §"等待口径明确"）是**结构性**的：正常上传路径不逐资源/逐批 `device_wait_idle` / `queue_wait_idle`；但池容量维护、swapchain 重建、退出排空这三类是低频维护动作，CPU 阻塞等待不仅正确而且应当——退出时"先等在飞工作完成再销毁"正是 fence/timeline host 等待的岗位。被淘汰的是"把阻塞当每一步的收口纪律"，不是"阻塞"这个原语本身。

### D3D12 对照

D3D12 的 `ID3D12Fence` 就是 timeline 语义的原生形态：`Signal(value)` 推进、GPU 侧 `CommandQueue::Wait(fence, value)`、CPU 侧 `SetEventOnCompletion(value)` / 查 `GetCompletedValue()`。frenderer 相当于"每画一个 mesh 就 CPU 等 fence 归零"的 D3D11 式 flush；本工程 3.2.4 是 value 流水。Unity 图形侧的 fence 语义同源。

## §3 现代引擎的做法

### 演进时间线

- **D3D11 / OpenGL 时代**：`UpdateSubresource` / `glBufferSubData` 把 staging 拷贝藏进驱动，应用手里没有"拷贝队列 + 进度值"这对玩具，引擎想流水也流不了——早期引擎普遍是 CPU 阻塞式收口，frenderer 是这个时代的忠实剪影。
- **D3D12（2014）**：fence value + copy queue 从第一天就是一等公民，引擎据此重构出流水化上传。
- **Vulkan 1.2（2019）**：收编 `VK_KHR_timeline_semaphore`；**1.3（2022）**：支持成为 mandatory（仍须显式启用，见 §1）。三家的同步模型自此事实上对齐。

### 四件套（三家共识）

1. **专用拷贝通道**：D3D12 copy queue / Vulkan transfer 队列 / Metal 多 queue。上传与图形分队。
2. **异步上传线程**：UE 的 async upload、Unity 的流送系统，在专门 CPU 线程攒上传批，与渲染提交解耦，主线程不为上传停车。
3. **进度值原语**：上传批推进 fence/timeline value，渲染在 GPU 侧等值；CPU 随时查进度做资源回收与背压。
4. **资源状态机**：placeholder → active 的流送切换，按帧预算逐帧灌数据。

### 各家落法

- **UE**：RHI 抽象 + RDG。UE5 把 RHI 显式拆成 graphics/compute/copy 多管线（`FRHIPipeline`），异步上传线程把资产序列化成拷贝命令进 copy 队列，完成信号用 fence value；pass 间 barrier/布局转换由 RDG 统一插入。
- **Unity**：图形层不开源，可确认的形状是 GfxDevice 抽象下各后端各自实现；D3D12 后端走 fence value + copy queue；mipmap streaming、Unity 6 GPU Resident Drawer 背后就是 upload heap 攒批 → 推 fence → GPU 等值。`AsyncGPUReadback` 这类 API 的存在本身就要求 timeline 类语义（读回是带进度值的等待）。

（内部类名与版本细节不编——上面是公开资料层级的形状描述。）

### 映射表

| 维度 | frenderer | 本工程 3.2 | 现代引擎 |
|---|---|---|---|
| 等待位置 | CPU 阻塞（wait_idle×2） | GPU 等票据值 | GPU 等 fence value |
| 批窗口 | 无（收集即消费） | 帧级合批 + 票据 | 上传线程攒批 + 预算 |
| 并行度 | CPU/GPU 串行 | 上传与渲染在飞 | 全流水 |
| 缺件 | —— | 上传线程、状态机、预算节流（步骤 4+） | —— |

因果链收束：**判定线"连续加载零 wait_idle"（超越 frenderer）→ 禁止 CPU 阻塞收口 → 等待搬进 GPU → 需要跨提交依赖 → 多批在飞要进度语义 → timeline 信号量（+ 显式启用 + synchronization2）**。每一环是上一环的必然，第一环是路线图里"超越 frenderer 每primitive两次wait_idle"那句话。

## §4 证据与边界

- 探针 `ash_renderer/examples/timeline_probe.rs` 的 A/B PASS 是**驱动行为观察**，保留为记录；它无验证层、两组均未启用 `synchronization2`，不能作为规范合规证据，待按[缺陷文档](../材料/已实现缺陷与修复验收.md)修正（隔离变量、启用验证层、正确声明能力）后作为验收工具复跑。
- 验收口径（3.2 README §验证）：验证层实际开启且目标路径零 WARNING/ERROR；timeline 覆盖同族/异族依赖分支；本机跑不到的回退路径注明未实测。
- 本机验证层尚未安装（启动日志既有 ERROR：未找到 `VK_LAYER_KHRONOS_validation`），3.3 前"装 SDK 补验证层"待兑现；缺环境不得用"无告警"通过。
