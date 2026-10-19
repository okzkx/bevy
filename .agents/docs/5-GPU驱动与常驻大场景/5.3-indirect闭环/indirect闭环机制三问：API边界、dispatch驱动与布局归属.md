# indirect闭环机制三问：API 边界、dispatch 驱动与布局归属

> 2026-10-19 收官后问答钉（用户复述校准轮），三问的结论性记录。正文机制走读见[流程导读](indirect闭环流程：从账本行到一次绘制提交的各级账面.md)；本文只钉"边界在哪"——Vulkan 给了什么、谁在驱动、布局谁说了算。

## 一问：所以 Vulkan 提供的就是一个 IndirectDraw API，其余都是我们自己写的？

**对，完全正确。** Vulkan 侧就一个 API + 一张 buffer：

```c
vkCmdDrawIndexedIndirect(buffer, offset, drawCount, stride)
```

它做的事：**在 GPU 执行时**，从 buffer 里连读 drawCount 条 `VkDrawIndexedIndirectCommand`（20B/条），逐条当成一次 `vkCmdDrawIndexed` 的参数——参数结构就是把 `vkCmdDrawIndexed` 的五个函数参数搬进显存。没有剔除、没有组装、没有压缩、没有排序。

这个 API 唯一的本质变化：**draw 参数的取值时刻从"CPU 录制时"挪到"GPU 执行时"**。"GPU 自己决定画什么"的能力全是引擎自建的三样东西——可见性判定（cull.wgsl，5.2）、命令组装（build_indirect.wgsl）、在场清单（DrawList 走访 + 在场环）；消费端只有这一个 API。

Vulkan 给的另外两类东西都是**契约，不是能力**：feature 位（multi_draw_indirect）、maxDrawIndirectCount 上限、INDIRECT_BUFFER usage、屏障的 DRAW_INDIRECT 阶段/INDIRECT_COMMAND_READ 访问、firstInstance 的 InstanceIndex 语义——用法的语法规则，能力本身零提供（位不开，调用违 VUID，驱动可能照跑但验证层当场抓）。API 买到的东西只有"执行时取数"这个时间点本身：CPU 录制时不知道（红线也不许知道）每个槽可不可见，GPU 执行到那条 draw 时命令已在显存里——**决策点后移，CPU 从"每帧 N 次调用的决策者"退位为"每帧 1 次调用的供数者"**。Bevy 在这之上还要盖批集/动态槽位/压缩合批，因为 indirect 只认"数组里有什么画什么"，谁放、放多少、放得紧不紧全是引擎的事；我们的固定槽位一比一展开是最小可用版，等 5.4 规模上去、instanceCount=0 空条成为浪费大头，才轮到"实际条数只有 GPU 知道"的形状（vkCmdDrawIndexedIndirectCount + count buffer）。

## 二问：build_indirect dispatch 是"特地调用"吗？谁调它？

**是帧级常驻的自动驱动，不是人工触发。** `frames.rs::record_frame` 里渲染进场之前固定顺序两个 dispatch：

```rust
if let Some(cull) = &draw.cull         { cull::record(...) }      // ① 剔除：写标记
if let Some(indirect) = &draw.indirect { indirect::record(...) } // ② 组装：写命令
```

`draw_frame` 每帧组装 `FrameDraw` 时决定要不要录：**在场行数 > 0 就录，= 0（空场/全暂缓）整个不出现在命令缓冲里**——count=0 短路，不需要谁记得去调。它和 cull 同位置（图形命令缓冲内、渲染进场之前），同族同队列，靠命令缓冲顺序 + 屏障排依赖。

**为什么不并进 cull 顺带写命令**（计划待决 1 落定）：①写职责不挤一个 shader——cull 管可见性、组装管命令，同构于 Bevy `mesh_preprocess` → `build_indirect_params` 两步；②cull 可以关（A/B 开关），关时 cull dispatch 整个缺席（host 填全可见标记），但组装每帧都要有命令可读——挤在一起就得往 shader 里传开关语义；③cull 管线布局不动（5.2 首钉"组号随管线布局"的教训）。代价 = 多读一遍标记 + 一次 dispatch 开销，小场景可忽略。

## 三问：ObjectRecord 和 IndirectCommand 结构是 Vulkan 要求的吗？

**一半一半。IndirectCommand 的布局规范钉死；ObjectRecord 纯自定义。**

**IndirectCommand = `VkDrawIndexedIndirectCommand` 的字节镜像**——字段序与语义规范定死，因为它就是 vkCmdDrawIndexed 的参数表：

```c
typedef struct VkDrawIndexedIndirectCommand {
    uint32_t indexCount;
    uint32_t instanceCount;
    uint32_t firstIndex;
    int32_t  vertexOffset;     // 规范里就带符号（允许负偏移做实例复用）
    uint32_t firstInstance;
}                              // 20B，stride 需 4 的倍数
```

驱动按**固定偏移**硬解这 20B。WGSL 侧 `IndirectCommand` 只是这个 C 结构的镜像（5×u32 同序，vertex_offset 用 `bitcast<i32>` 塞同 4 字节——池引脚值域恒正，位型对上即可），`indirect_command_layout_mirror` 单测钉镜像关系；错一位，GPU 画出来就是乱的或越界。

**ObjectRecord = 完全私有契约**：对象表是我们自己的 storage buffer，48B 布局（三池引脚/材质槽/局部 AABB）是我们定的，Vulkan 对内容零要求（只要求 binding 是 STORAGE_BUFFER）。Rust 侧 `pack_object_record` 手工排字节、WGSL 侧抄同一份结构，两份靠 `ObjectRecordLayout` 静态断言互证——改布局 Vulkan 不会拦，只有我们自己的互证断言在管。

**分界线一句话**：只有"Vulkan 要亲手读的数据"（indirect 命令、顶点属性、UBO、索引类型这类进固定管线状态的）布局才由规范钉死；凡走 descriptor 进 shader 的（storage/uniform），布局是我们和自己的 WGSL 之间的私有契约。`IndirectCommand` 特殊在两头都沾——GPU 写时是普通 storage buffer（descriptor 面），驱动读时按规范布局硬解（固定管线状态面），所以两头都得贴规范。
