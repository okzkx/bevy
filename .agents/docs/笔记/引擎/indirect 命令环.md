# indirect 命令环：drawCount、instanceCount 与剔除记账

Hi-Z 剔除之后 CPU 不知道实际画了几个实例，`vkCmdDrawIndexedIndirect` 却要求 CPU 给出条数——两者并不矛盾：剔除结果只写进每条命令的 `instanceCount` 字段（0 或 1），命令环长度 `drawCount` 始终是 CPU 账本里的可绘行数，有效绘制数 V = `Σ(instanceCount > 0)` 只存在于显存。本篇讲清这条命令环的两数账、`instanceCount = 0` 的管线语义与身份传递方式；成本与粒度的正本在 6.1 段《遮挡剔除的成本、覆盖与粒度》，本篇补的是它之外的 API 账目半边，不重复。

## §0 判定线

- **两个数不同账**：`drawCount`（命令环长度 N，剔除前）由 CPU 录制时给出 = 在场账本可绘行数；有效绘制数 V = `Σ(instanceCount > 0)`（剔除后）**只存在于显存**。`vkCmdDrawIndexedIndirect` 只要求 CPU 知道 N，不要求知道 V——两者同时成立，无矛盾。
- **`instanceCount = 0` 的管线语义**：该次 draw 不产生任何顶点着色器调用、不生成三角形、不进入光栅化——几何侧 + 片元侧全省。被剔除实例只付 GPU 前端解析一条 20B 命令的成本（百 ns 级）。
- **CPU 不知道 V 是设计使然**：N 来自 CPU 自己的账本（合法），V 若跨回 CPU 就是决策权回流（回读墙）；诊断读数（`cull_stats`）只进计量不进决策（读数不投票）。
- **记录单位 = 实例**：对象表 1 行（slot）= marks 1 位 = 命令环 1 行，`instanceCount ∈ {0,1}` 恒成立；同 mesh 的多个实例是对象表多行，各占各的 slot，可见性逐实例裁决。
- **命令内容的产地**：5 个字段没有一个在录制期由 CPU 写进命令——CPU 提前发布原料行（在场环/对象表/LOD 表），GPU compute 拼装，图形执行期按命令自取。`vkCmdDrawIndexedIndirect` 录制期只递 buffer 句柄 + offset + `drawCount` + stride。
- **升级判定线**：只有把命令环压缩成"仅可见行"（环长 = V）时，条数才变成只有 GPU 知道，才需要换 `vkCmdDrawIndexedIndirectCount`（`draw_indirect_count` 特性）。当前规模不值得。

## §1 两个数：N 与 V

```c
vkCmdDrawIndexedIndirect(cb, buffer, offset, drawCount /* = N */, stride /* = 20B */);
```

GPU 前端从 `buffer[offset]` 起顺序消费 `drawCount` 个 `VkDrawIndexedIndirectCommand`（indexCount/instanceCount/firstIndex/vertexOffset/firstInstance，5 × u32）。三个量的精确分工：

| 量 | Vulkan 名字 | 值 | 谁知道 |
|---|---|---|---|
| 命令环长度 | `drawCount` 参数 | N = 在场账本行数 | **CPU**（InstanceLedger/DrawList 枚举，权威镜像，零回读） |
| 每条画几个 | `instanceCount` 字段 | 可见非透明 = 1，被剔/透明 = 0 | **GPU**（cull + Hi-Z → marks → build_indirect 现写） |
| 有效绘制数 | V = Σ(instanceCount>0) | 无独立存储，隐含在命令环里 | **仅 GPU**（CPU 只能经诊断回读拿旧账） |

关键：Hi-Z 的剔除结果**只写进 `instanceCount` 字段**，命令环的长度从不由剔除结果决定。所以"CPU 不知道剔除结果"与"`vkCmdDrawIndexedIndirect` 要求 CPU 给条数"同时成立——它要的是 N，不是 V。

## §2 `instanceCount = 0` 省的是哪段账

一条 draw 的 GPU 工作三段，剔除行只付第一段：

| 段 | 工作 | instanceCount=0 时 |
|---|---|---|
| ① GPU 前端 | 解析命令、发起 draw | 照付（判零退出，约百 ns/条） |
| ② 顶点处理 | 顶点 fetch、VS、图元装配 | **零**（不产生任何顶点调用） |
| ③ 光栅化+着色 | 三角形设置、FS 逐片元 | **0**（无三角形可光栅化） |

所以剔除省的是 ② + ③ 全部，不是"只省着色"；多付的是 ① 空命令前端扫描 + cull/build_indirect compute 本身。与 early-Z 的分工（early-Z 出手前几何侧全链已跑完，只能省像素侧；Hi-Z 让几何侧归零）见 6.1 成本篇，此处只钉管线语义。

## §3 身份传递：记录单位 = 实例，firstInstance 内嵌

```
1 个实体（ECS） = 对象表 1 行（slot）= marks 1 位 = 命令环 1 行（instanceCount ∈ {0,1}）
```

- 可见性记录的单位不是 mesh，是**实例**——同 mesh 的 N 个实例是对象表的 N 行，各占各的 slot，各自 model 矩阵，marks/命令环逐行独立，Hi-Z 逐实例裁决（"B 处的桥墩被挡、A/C 可见"）。
- 身份不走映射表：命令自带 `firstInstance = slot`，VS 里 `@builtin(instance_index) == firstInstance == 槽号`，按 slot 读对象表（`indirect.rs`："图形侧无 push"）。
- mesh 数据不重复：N 行命令的 `firstIndex/vertexOffset` 指向 MeshPool 里同一份顶点/索引——显存共享一份，重复的只是 20B 命令头。
- 合批对照（`instanceCount > 1` 的传统 instancing 形状）：需要 per-instance buffer 按 `SV_InstanceID` 索引取参数，省 drawCount 但**合批内实例同生死**，逐实例剔除成果漏掉——与"剔除粒度必须跟能省掉的账单同粒度"（6.1）冲突，故不取。

**命令消费的三层分工**（谁在读、读什么，各不相同）：

| 内容 | 谁消费 | 从哪取 |
|---|---|---|
| mesh 几何（取哪段索引/顶点） | **固定功能前端**（不是 shader） | 命令环 5 字段 → 绑定的顶点/索引池；索引 fetch、顶点 fetch、装配全在可编程管线之前 |
| 实例身份 → 变换/材质槽 | VS | `objects[instance_index]` / `models[instance_index]`（set2，id = `firstInstance` = 槽号） |
| 贴图/采样器 | FS | 材质环按 `material_index` → bindless 大数组按记录内槽号（set0）——id 套 id |
| 帧常量（view_proj 等） | 全体 | set1 帧 UBO，绑定即得，无 per-instance 寻址 |
| 光源列表 | FS | 分簇集按屏幕 tile + 深度片寻址（per-pixel，非 per-instance） |

一句话：命令环只负责"这个实例是谁（槽号）+ 用哪段几何（池内 pin）"，其余全是 shader 沿槽号在常驻表里自己走的 id 链——**每实例的差异全部住在表里，命令和 shader 都不重复携带**。

## §4 两份知识、一次拼装

CPU 发布的是**数据行本体**（不是指针——显存里没有指回 CPU 的东西），GPU 在两个阶段各取各的：

```
CPU 权威知识（帧首/事件驱动发布）
  在场环：可绘槽号数组（write_presence_ring，drawlist.rs:238）
  LOD 表环：pack_lod_entry(first_index, index_count, vertex_base)（:272）
  对象表/材质环：model 矩阵、mesh 引用、材质槽
GPU 现场知识
  cull compute：视锥 + Hi-Z → marks（只有 GPU 知道）
        ▼
阶段一（组装）  build_indirect：在场环 × marks × 对象表/LOD 表 → 命令环
        │ HANDOFF 屏障（DRAW_INDIRECT + INDIRECT_COMMAND_READ，record/compute.rs:32）
阶段二（执行）  图形前端逐条消费命令：firstInstance → 对象表行；
        firstIndex/vertexOffset → 顶点/索引池；bindless 槽号 → 纹理表
```

CPU 全程只做两件事：发布行数据 + 录制一条"读那个 buffer、共 N 条、每条 20B"的命令。它不看内容，也不知道谁最终被画——**录制期给位置，执行期才取内容**，这就是 indirect 的全部意义。

build_indirect 是独立 pass（独立 dispatch），不是 cull 顺带写命令，三个理由：①**职责不挤一个 shader**——cull 管可见性、组装管命令，同构于 Bevy `mesh_preprocess` → `build_indirect_params` 两步；②**开关面不对称**——cull 可以关（A/B 开关，关时 dispatch 整个缺席、host 填全可见标记），但组装每帧都要有命令可读，挤在一起就得往 shader 里传开关语义；③**布局纪律**——cull 管线布局不动（5.2 首钉"组号随管线布局"的教训）。代价 = 多读一遍标记 + 一次 dispatch 开销，小场景可忽略。

命令消费的屏障还有一条与惯例不同的钉子：**indirect 的读者不是 shader stage**，屏障 dst 必须开到 `DRAW_INDIRECT` 阶段 + `INDIRECT_COMMAND_READ` 访问（"屏障 dst 开到读方 shader stage"的惯例在此不适用）；它与其上"标记写→组装读"的预置屏障（dst `VERTEX|COMPUTE_SHADER`）拼成两段依赖链。

## §5 何时升级 DrawIndirectCount（未立项）

把 build_indirect 升级成"前缀和压缩成仅可见行 + 末尾原子写 count"，`drawCount` 变成执行期从 countBuffer 现读（`vkCmdDrawIndexedIndirectCount`，`maxDrawCount` 硬上界）。它是同一条"决策点后移"逻辑的自然延伸——`vkCmdDrawIndexedIndirect` 后移了"每条画几个"，这一步把"画几条"这个最后的 CPU 侧常量也后移到 GPU 执行时。取舍判据：命令条数上界本来就是 CPU 在账信息（在场行数）时，后移无必要——本工程选 `cmd_draw_indexed_indirect`（CPU 传在场行数上界 N）；翻案条件 = 压缩合批后 `instanceCount=0` 空条成为浪费大头。收益 = 省空命令前端扫描 + CPU 免算上界；代价 = compute 侧压缩趟 + count 写读依赖 + `draw_indirect_count` 特性门 + CPU 失去实际条数（诊断改走回读）。**判定线（间接 `indirect.rs:9-10` 已钉）：条数只有 GPU 知道时（压缩合批）才需要**——当前规模（万级行、空条判零退出）不触发。

## §6 vkCmdDrawIndexedIndirect 的 API 形状

```c
void vkCmdDrawIndexedIndirect(
    VkCommandBuffer cb,
    VkBuffer       buffer,    // INDIRECT_COMMAND 用途的 buffer
    VkDeviceSize   offset,    // 4 对齐
    uint32_t       drawCount, // 条数（N，录制期常量）
    uint32_t       stride);   // 条距，4 的倍数，允许 > 20B（垫开对齐）
```

**结构体本身被规范冻结**：`VkDrawIndexedIndirectCommand` 是 `vulkan_core.h` 里的 C 结构体，字段、顺序、大小全定死——5 × u32 = 20B，全 u32 无对齐坑。没有版本号、没有自描述头，**布局就是 ABI 契约**：GPU compute 按这个序写原始字节，驱动前端命令解析器按同一布局读，这是 shader 字节镜像（`instance.wgsl`）合法的根据。会变的只是周边参数（条数/条距/偏移/特性门：`drawCount > 1` 需 `multi_draw_indirect`）。

```c
typedef struct VkDrawIndexedIndirectCommand {
    uint32_t indexCount;     // 0B  本条画多少索引（本工程 = LOD 档位 pin 的 index_count）
    uint32_t instanceCount;  // 4B  画几个实例（本工程恒 0/1：可见 = 1，被剔/透明 = 0——GPU 侧杀 draw 的开关）
    uint32_t firstIndex;     // 8B  索引池内起始索引（本工程 = LOD pin）
    int32_t  vertexOffset;   // 12B 顶点池内偏移（本工程 = LOD pin 的 vertex_base）
    uint32_t firstInstance;  // 16B 实例起始号（本工程 = 实例槽号，VS 按 instance_index 读对象表）
} VkDrawIndexedIndirectCommand;
```

五个字段就是相对 `vkCmdDrawIndexed` 的全部实参——一次 indirect 批内，pipeline/descriptor set/顶点索引绑定对 N 条统一生效，这五个字段是唯一的 per-draw 变量；`vertexOffset` 是有符号的（负值合法），其余无符号。

同族兄弟结构各自固定、互不通用：`VkDrawIndirectCommand`（非索引，4 字段 16B）、`VkDispatchIndirectCommand`（3 字段 12B，`cmd_dispatch_indirect` 用）；`DrawIndirectCount` **不换结构**，只把条数来源换成 countBuffer 现读。真正引入新命令格式的是 DGC（`VK_EXT_device_generated_commands`，预处理命令序列，另一套体系）。

## §7 Unity 用法对照（官方文档核实）

Unity 的间接绘制 API 表面同形，语义不同——**buffer 内容和 Vulkan 同形，但不是可执行命令流**：

- **旧 API `Graphics.DrawMeshInstancedIndirect`（已标 obsolete）**：`bufferWithArgs` 必须含 5 个整数，字段序与 `VkDrawIndexedIndirectCommand` 相同（index count per instance / instance count / start index location / base vertex location / start instance location，D3D 命名）。但一次调用 = **一个 CPU 点名的 Mesh + Material 的一次绘制**，buffer 只给这一次 draw 的参数；bounds 也由 CPU 传（整批包围盒，文档明说实例不再被视锥/遮挡剔除）。
- **现代 API `Graphics.RenderMeshIndirect`**：commandBuffer 存 `GraphicsBuffer.IndirectDrawIndexedArgs` 结构体（文档警告布局随平台变，须用结构体、不可摆原始 int），一次调用可执行 `commandCount` 条命令——但仍是"**同一个 Mesh 画多次**"，每条命令只有数量类参数，mesh 身份在 CPU 参数里。
- **本质差**：Vulkan 环是**命令流**，每条自带 mesh 身份（`firstIndex/vertexOffset` 指向池内不同 mesh、`firstInstance` 带实例槽号），一次调用画 N 个不同 mesh；Unity buffer 是**指定 mesh 的参数表**——谁画（mesh/material/bounds）是 CPU 每次调用声明的，buffer 只定"画几次/从哪取索引"。所以 draw call 数：本工程不随对象数增长；Unity 的 GPU-driven 形状下仍随 mesh 种类（批次）数增长，后移的只有 instanceCount（现代 API 加了 commandCount 维度，但限同一 mesh）。
- **per-instance 身份同构**：Unity shader 靠 `unity_InstanceID` + 自带 `StructuredBuffer`（需 include `UnityIndirect.cginc` 声明 `UNITY_INDIRECT_DRAW_ARGS`）；本工程 `firstInstance = 槽号` 直读对象表。两者都是"GPU 数数，CPU 不管"，差的只是身份载体归谁（引擎内置约定 vs 命令内嵌）。

一句话：Vulkan 把"选哪个 mesh、哪个管线、哪些实例"全部放进 buffer；Unity 把这些留在 C# 参数里，buffer 只放数量。这就是"Unity 里 Buffer 不是绘制指令"的准确含义——同一串 20B 字节，消费方一个是 GPU 前端，一个是 Unity 渲染器给固定 mesh 挑参数。

## 涉及源码 / 来源

- `ash_renderer/shaders/common/mesh.wgsl`——id 链消费端：`mesh_vertex` 按 `instance_index` 读对象表/模型环，`sample_surface` 材质记录 → bindless 大数组
- `ash_renderer/shaders/build_indirect.wgsl:63-70`——instanceCount 产地（可见/透明/被剔三态 select）
- `ash_renderer/src/render/frame/frame_draw.rs:327`——`cmd_draw_indexed_indirect(cb, buf, 0, indirect_count, 20B)`
- `ash_renderer/src/driver/host/frame.rs:204`——`indirect_count = assembly.slots.len()`（N 的出处）
- `ash_renderer/src/render/passes/visibility/indirect.rs:9-14`——条数判定线、firstInstance 携带槽号、instanceCount 恒 1/0
- `ash_renderer/src/driver/host/drawlist.rs:238/:272`——在场环发布、mesh 偏移打包（CPU 原料发布）
- `ash_renderer/src/render/resident/tables/instance_table.rs:15`——命令环角色与写读权
- Vulkan API 形状与 Unity 对照：Vulkan 1.3 规范 `VkDrawIndexedIndirectCommand` 定义；Unity 官方文档 `Graphics.DrawMeshInstancedIndirect`（bufferWithArgs 五整数、obsolete 指向 RenderMeshIndirect）与 `Graphics.RenderMeshIndirect`（IndirectDrawIndexedArgs 结构体、commandCount 多命令、同一 Mesh）——2026-11-04 核实
- 姊妹篇：[GPU-Driven 本质](GPU-Driven%20本质.md)（决策点后移与控制面/数据面分家）、[为什么没有全上 GPU 驱动](为什么没有全上%20GPU%20驱动.md)（读数不投票与回读墙）、6.1 段《遮挡剔除的成本、覆盖与粒度》《Hi-Z 遮挡原理》（成本/粒度/滞后正本）
