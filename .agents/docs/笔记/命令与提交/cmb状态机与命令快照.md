# cmb 状态机与命令快照：bind 不上设备时间线

录制一段命令：`bind_pipeline(A)`、一条 draw、`bind_pipeline(B)`、再一条 draw，然后提交。GPU 的执行流里找不到任何 bind 动作——它拿到的只有两条各自带齐管线、描述符、顶点绑定等参数的 draw。**命令缓冲区（cmb）因此不是待执行命令的裸数组，而是"状态机 + 逐条命令的参数快照"：bind 系列在录制当场修改 cmb 的当前状态，draw/dispatch 在被记录的那一刻把此刻状态整体拷进自身，bind 本体不产出任何 GPU 可执行命令。** 提交给 GPU 的是一串"自带全套参数的命令记录"——执行顺序与屏障只管 draw/dispatch 这类真正干活的命令之间的事，与 bind 无关。

## 1. 命令四分类

规范（§Fundamentals "Command Properties"）把命令分四类，各类在 cmb 里的形态完全不同：

| 类 | 规范定义 | 例子 | 在 cmb 里的形态 |
|---|---|---|---|
| 动作（action） | "perform operations that can update values in memory" | draw、dispatch、copy、fill | GPU 可执行事件，执行序由依赖关系定 |
| 状态（state） | "update the current state of a command buffer, affecting the operation of future action commands" | bind_pipeline、bind_descriptor_sets、bind_vertex/index、push constants、cmd_set_*（dynamic state） | 不进设备时间线；效果写进后续执行命令的参数 |
| 同步（synchronization） | "impose ordering constraints on action commands, by introducing explicit execution and memory dependencies" | pipeline barrier、事件等待 | GPU 可执行事件，纯排序、不碰数据 |
| 间接（indirection） | "execute other commands which were not directly recorded in the same command buffer" | indirect 命令的消费（从缓冲区取参数） | 嵌在执行命令的参数侧 |

表里第一类"动作（action）"就是 draw、dispatch、copy、fill 这些真正读写数据、提交后会被 GPU 执行的命令；它们之间才存在数据依赖，也才需要屏障。状态命令不进执行时序，没有被屏障管理的资格——"bind 要不要加屏障"本身是个不成立的问题。

规范对状态命令的定义是 "affecting the operation of future action commands"。这里的 future 指**录制顺序在后**，不是执行时靠后；规范默认读者已经知道状态命令不进设备时间线，没有点破这一点，这是最大的误读陷阱。

## 2. 快照语义

规范 note 原文："Each action command **uses the state in effect at the point where the command occurs** in the command buffer"——draw、dispatch 这类命令按它在命令流中的**位置**取状态快照，与它何时执行无关。

录制 dispatch 时，cmb 把当下整排寄存器状态整体拷进这条命令：pipeline、描述符集、push constants、顶点/索引绑定、dynamic state、当前渲染实例的附件组。快照之后对状态的一切改动都与这条记录无关。类比：录制像填表格，bind 是换笔，dispatch 是落笔写字——交上去的作业上只有字迹，笔没有一起交。

快照没有追溯性：bind 只影响之后录的命令，不影响之前录的；同一条 cmb 里"bind A、dispatch、bind B、dispatch"交替，两条 dispatch 的快照互不干扰。

## 3. 两条时间线

| 时间线 | 谁在走 | 顺序性 | 状态命令在吗 |
|---|---|---|---|
| 录制期 | CPU 逐行跑录制代码 | 严格程序序 | ✓ 就发生在这里 |
| 执行期 | GPU 异步消化命令流 | 只由屏障/信号量决定 | ✗ 根本没有 bind 这种事件 |

"异步"只形容 **draw、dispatch 这类真正执行的命令**。bind 和 barrier 一样都是 CPU 录制时逐个写入 cmb 的内容，区别只是写入的内容：barrier/dispatch 写"给 GPU 执行的指令"，bind 写"后续指令的参数"。执行期只有已经拍好照的命令——快照在提交之前就已定死，不存在执行期补拍。

"bind 先于 dispatch"由录制期的程序序保证，不由执行期保证：录制是单线程顺序代码，bind 当场把状态写进 cmb 内存，紧随的 dispatch 当场取走快照；到 `queue_submit` 时 cmb 已经是一块定死的内存——执行期没有 bind 这种事件，"bind 与 dispatch 谁先到 GPU"的竞争根本不存在。

## 4. 提交与队列：按提交登记，命令流按序消费

`queue_submit` 之后 cmb 经历什么——两个常见误猜都错：**"cmb 拆开、每个真命令入队"** 与 **"第一道 barrier 前的被切出来并发跑"**。真相是三层；先给结论：**队列不存命令，只存对提交（一组命令缓冲）的引用；cmb 也不拆包，执行时整块按序消费。**

**队列层：按提交登记。** `vkQueueSubmit` 的参数是 `VkSubmitInfo` 数组，每个 batch = 一组命令缓冲；fence、信号量全挂在提交上。队列里存的是对 cmb 内存的**引用**，不是拍扁的命令——Windows WDDM 的 GPFIFO（GPU FIFO）字面上就是"pushbuffer 指针的 FIFO"。规范侧对仗：提交后 cmb 进入 **pending** 状态（"as it may be executing on the device"——设备执行的是这份记录本体，应用禁止改动），执行完回 executable（ONE_TIME_SUBMIT 回 invalid）。

**硬件层：命令处理器按序读取。** GPU 的命令处理器（AMD ME/PFP，NVIDIA host front-end）从 cmb 头按序读取、逐条解释、逐条放行——draw 发图形引擎、dispatch 发计算引擎，读下一条不等上一条跑完。barrier 是流里的一条指令：读到它命令处理器停下来等，过去之后继续。**并行的边界由执行中读到的 barrier 指令画出，不在提交时刻预先切分**——段界空隙、引擎重叠、两帧在飞，全是这条单一读取流水线跑起来后自然涌现的重叠。

**翻译层：真正的"拆"在录制期。** 驱动把 VK 命令当场翻译成硬件原生命令包（AMD PM4、NVIDIA patch 过的 pushbuffer），secondary cmb 也是录制期语义嵌入（`vkCmdExecuteCommands` = "as if recorded in the primary"）。提交时被消费的就是翻译完的成品，不再改形。

一句话收拢：**提交 = 把整批命令缓冲（连同其中全部 barrier 指令）一次性交给队列；GPU 的命令处理器按记录顺序逐条读取、逐条放行，读到 barrier 就在那里停下等待。**

## 5. 状态是寄存器台，不是一个游标

cmb 的"当前状态"是一组带槽位的寄存器，bind 系列各管各的：

- **pipeline**：graphics 与 compute 两个绑定点各一份——② 段 dispatch 与 ④ 场景 draw 的绑定互不干扰的原因；
- **描述符集**：每个 set 号一个槽，绑 set0 不动 set1；
- **顶点/索引 buffer 绑定、push constants、dynamic state**：viewport/scissor 等 `cmd_set_*` 同为状态命令，同批处理；
- **渲染实例状态**：`cmd_begin_rendering` 开出的"当前附件组"，录制 draw 时一并拷走。

## 6. 工程推论（ash_renderer 落点）

1. **绑定成本记在 CPU 录制侧，不在 GPU 时序侧。** bindless"整帧绑一次"（场景管线三 set 绑一次管几千个 draw）省的就是寄存器台的反复重拨。
2. **pipeline 切换的 set 槽兼容是状态机槽位规则。** 换 pipeline 后新快照能否继承旧寄存器值，看 layout 兼容性——overlay 管线自有 layout，切过去之后必须重绑 set0（`render/passes/overlay/overlay_pipeline.rs` 的重绑注释）。
3. **compute dispatch 的交接屏障靠类型系统强制。** `core/record/compute.rs` 的 `ComputeHandles::dispatch` 签名强制带 `Handoff` 参数，dispatch 后自动补一道内存屏障——"必须补屏障"不靠自觉，靠编不过。

## 自测四问

1. 为什么 bind 类命令不需要任何屏障，也永远不可能和 dispatch 竞争？
2. "bind A → dispatch → bind B → dispatch"两条 dispatch 各用什么管线？谁在什么时候定的？
3. overlay 切管线后重绑 set0 的必要性，用寄存器台的语言说明。
4. 提交后队列上排队的是什么？第一道 barrier 前的命令会被提交时切出来并发跑吗——如果不会，并行边界是谁画的？

## 来源

- 规范 §Fundamentals "Command Properties"——四分类定义与快照 note 原文
- 规范 §Synchronization "Submission Order"（隐式同步保证）——状态命令"在 host 记录时执行、不在设备上执行操作"原话
- 规范 §Command Buffers（生命周期与提交节）——pending 状态语义、提交批次=命令缓冲组、ONE_TIME_SUBMIT 状态迁移
- GPU 命令处理器模型——AMD ME/PFP、NVIDIA host front-end 按序消费命令流；WDDM GPFIFO = pushbuffer 引用的 FIFO（硬件资料侧知识，非规范条款）
- [同步模型](../同步与队列/同步模型.md)——draw/dispatch 这类命令之间的执行/内存依赖细则、屏障与信号量的分工
- [多帧在飞竞争](../同步与队列/多帧在飞竞争.md)——cmb 之上提交/队列层的竞争与等待关系
- [整帧录制流程](整帧录制流程.md)——本工程九段录制编排，以及命令流里 bind 与 barrier 的分工
- 工程代码：`core/record/compute.rs`（Handoff 交接屏障收拢）、`render/frame/record.rs`（整帧录制编排、overlay 切管线重绑 set0、bindless 整帧一绑；`UploadWait` 定义在 `render/frame/frame_pool.rs`）
