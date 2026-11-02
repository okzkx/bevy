# bindless机制：从换绑到索引——frenderer锚点与Unity批处理对照

> 2026-09-28。3.3 段收官后的机制讲解沉淀，回答四个递进的问题：① bindless 到底改变了什么（以 frenderer 的 DescriptorSet 用法为锚）② 游戏开始要加载所有纹理吗 ③ 与 Unity SRP Batcher / GPU Instancing / Instanced Indirect 是什么关系 ④ Unity 是不是没做 texture bindless。
>
> 分工：[描述符索引特性族篇](描述符索引特性族：能力位全景、三层配套与限额双轨.md)管"许可怎么开"（规范面），[3.3.2-3.3.4 施工记录](3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路.md)管"代码怎么落"（实现面），**本篇管"为什么这么设计"**。frenderer 已归档仅作 A/B 基准（源码在 `F:\okzkx\rust-frenderer`），引用它只为锚定已有经验。

## 0. 判定线

**bindless 的本质换位只有一句：资源选择权从 command buffer 的绑定状态（换 set）搬进普通着色器输入数据（换一个 u32）。其余一切——特性位、UAB、partially bound、采样器去重、retire 契约——都是这个换位的工程保障。**

## 1. 锚点：frenderer 的传统 set 形状

传统模型的教科书循环，正是 frenderer 自己的代码（`modules/remote/renderer/src/render_params.rs` 的 `RenderParams::render`）：

```rust
for element in elements {
    let descriptor_sets =
        render_tool.get_sets_from_map(create_descriptor_id_set_map(&element)); // ① 按材质查 set
    ...
    device.cmd_bind_descriptor_sets(..., &descriptor_sets, &[]);              // ② 逐 element 绑
    device.cmd_bind_vertex_buffers(...); device.cmd_bind_index_buffer(...);
    device.cmd_push_constants(...);                                          // ③ 逐 element 推参数
    device.cmd_draw_indexed(...);                                            // ④ 画
}
```

配套的 `descriptor_with_set.rs`：每个材质一套 set，创建时 `update_descriptor_sets` 写入一次就冻结，Drop 时 free。

这个模型里，**"这个 draw 用哪张贴图"的答案写在 command buffer 的绑定状态里——换资源 = 换 set**。`DescriptorIndex → DescriptorSetsIndex` 那张 CPU HashMap，就是"材质身份 → 绑定状态"的查询表。

## 2. 换位：答案从绑定状态搬进数据

bindless 把同一循环变成：

```rust
for primitive in primitives {
    // 没有 ①：不再查 set——贴图身份只是一个数字
    // 没有 ②：set0 全帧绑一次（循环外），常驻不动
    device.cmd_push_constants(..., &push_bytes(model, slots[i].texture, slots[i].sampler, ...));
    device.cmd_draw_indexed(...);
}
```

shader 侧，`var t: texture_2d<f32>`（一张图）变成 `binding_array<texture_2d<f32>>` + `textures[push.tex_index]`（一整柜按下标取）。

**`DescriptorIndex→SetsIndex` 那张 HashMap 没有消失，它被搬进了 GPU**——CPU 侧的"材质身份查 set"变成 shader 里的 `textures[index]`。这就是名字里的 "less"：不是没有描述符，是**不再逐 draw 绑定描述符**。

## 3. 为什么传统模型做不到：四条契约与打破它们的开关

传统模型有一套隐含契约，frenderer 的代码默默遵守着；bindless 逐条解约，每条解约对应一类特性位（规范细节见[特性族篇 §2](描述符索引特性族：能力位全景、三层配套与限额双轨.md)）：

| 传统契约（frenderer 遵守的） | 打破它的 bindless 需求 | 对应开关 |
|---|---|---|
| 一个 binding = 一个资源 | 一个 binding = 1024 个资源的数组 | `descriptorIndexing` 家族 |
| set 创建后冻结（更新一次就不再动） | 运行中往表里塞新贴图 | `…UpdateAfterBind` |
| 绑定状态下内容不可变 | 表内容边画边改 | 同上（UAB） |
| draw 时全组线程看同一个绑定 | 每线程按自己的 index 取 | `…NonUniformIndexing` |

frenderer 的 `update_process_textures_to_descriptor_set`（换图）必须在帧外跑——契约说 set 在飞/在绑时不能动。UAB 特性位就是 Vulkan 把这条契约**单独解除**的授权：只对声明了旗标的 binding、只对启用了特性的设备生效。所以这套东西不是"新写法"而是"新许可"——硬件一直有这个能力，API 默认不给你。

## 4. 一帧的两种形状（并排）

```
frenderer（传统）                      ash_renderer 3.4（bindless）
─────────────────────                ─────────────────────────────
init: 每材质建 set、写入              init: 建 set0 表(1024槽) + 贴图到货 publish 进表
per draw:                            per frame:
  查 set(map)                          bind set0 + set1（一次）
  bind set(s)     ← O(draw) 次        per draw:
  push 常量                              push {model, tex_index, sampler_index}  ← 数据
  draw                                  draw
换贴图 = 换绑/更新 set                换贴图 = 换一个 u32
```

描述符对象数从 **O(材质)** 降到 **O(1)**，绑调用从 **O(draw)** 降到 **O(帧)**。这是引擎做 bindless 的第一层动机；第二层在 GPU-driven（本项目的步骤 5）——"用哪张贴图"既然是 shader 里的数据，GPU 就能自己决定它，CPU 侧那个查询表根本不存在了。

## 5. 传统 set 没有死：资源种类分裂

bindless 不是取消 DescriptorSet，是取消 **per-material 的那部分**。数据按"多久变一次"分流：

- **全帧一致**（相机矩阵）→ 传统 set（我们的 set1 每帧 UBO），绑定即生效，老模型就是对的；
- **逐 draw 才变的一小撮**（model + 两个索引 + base_color）→ push constant 96B；
- **逐 draw 引用的大资源**（贴图/采样器）→ 常驻表 + 索引。

frenderer 的逐 element bind 在当时不是错的——它只是为每份不一致的数据都付了一次绑定成本。bindless 把账重新分了。

## 6. "游戏开始要加载所有纹理吗？"——容量与占用是两个词

**不需要。一开始冻结的是容量（地址空间），生长的是占用（槽位内容）。**

- **建表时冻结**：数组能编址到多大（layout 的 `descriptor_count = 1024`）+ set 本体。此后表是空的（只有 fallback 占 0 号），但地址空间就位。
- **运行中生长**：新贴图随资产到货、上传批次提交成功后 `publish()` 占一个**没用过的新槽**。合法性来自三个运行时机制：UAB（绑着也能写）、PARTIALLY_BOUND（空槽合法只要不访问）、fallback 槽 0（缺资源的 draw 也有有效槽可引）。

M2 的 FlightHelmet 是静态场景，15 张图头三帧就全部到货发布（上传批次 #2 4 张 → #3 9 张 → #4 2 张——**本身就是分批到货、逐批发布**，只是间隔几毫秒）。换开放世界，同样的 `publish` 发生在几小时的游戏进程里，机制一行不改。"静态资产一次收齐"是内容属性，不是表的设计约束。

两个真约束（不是"要一次加载"，但要知道）：

1. **容量是硬顶**：同时驻留超 1024 张要走显式受控扩容（重建 layout/pool）或淘汰腾槽。表容量选的是"**峰值同时驻留数**"，不是"游戏总贴图数"。
2. **槽位回收要等最后使用完成**：写新槽零风险；把旧槽**腾出来再用**是 UAB 世界唯一真正危险的动作（在飞 draw 可能还在采样它）——`retire_*` 三条安全契约为此存在，完整淘汰账本归步骤 4。

用 Unity 类比：这更像**纹理流送（Texture Streaming）+ 固定尺寸数组**的组合——数组容量编译期定死，条目运行期换进换出；而不是"启动时把所有资源 Load 一次"的同步加载。

## 7. Unity 对照：SRP Batcher / Instancing / Indirect 各砍哪一刀

一次 draw 的成本分解：

```
每次 draw = ① 状态切换（shader/材质/贴图绑定） + ② CPU 提交开销 + ③ GPU 执行
```

| 砍哪刀 | Unity 技术 | Vulkan 真身 | 我们项目对应 |
|---|---|---|---|
| ① 材质数据切换 | **SRP Batcher** | 大 uniform/storage buffer + 每 draw 动态偏移 | push 96B（3.4）；常驻实例参数表（步骤 5） |
| ① 贴图切换 | （无用户可见对应物） | 常驻描述符表 + 索引 | **bindless 表（3.3 已完成）** |
| ② draw 数量（同网格重复） | **GPU Instancing** | `vkCmdDrawIndexed(instanceCount)` + 实例号取参 | 常驻实例参数表（步骤 5） |
| ② + 决策权下放 GPU | **DrawMeshInstancedIndirect** | `vkCmdDrawIndexedIndirect`（compute 写 args） | compute 剔除 + indirect（步骤 5） |

**SRP Batcher**：同一 shader 变体的所有材质，`UnityPerMaterial` 强制成同一内存布局、塞进一块 GPU 常驻大 buffer；draw 时绑定一次大 buffer、每 draw 只改一个小偏移。批处理条件最松（同 shader 变体即可，网格材质都可不同），**不减 draw 数**，砍的是状态切换。

**GPU Instancing**：网格 + 材质完全相同的 N 个实例合成 1 次 draw，shader 用 `unity_InstanceID`（Vulkan 侧 `gl_InstanceIndex`）取每实例参数。条件最严（同网格 + 同材质）。

**Instanced Indirect**：draw 五参数（indexCount/instanceCount/startIndex/baseVertex/startInstance）也不在 CPU 手里，住在 GPU buffer——通常是前一个 compute pass 写的（剔除后把活着的实例写死在 args 里）。CPU 发 draw 时不知道要画几个。解锁 GPU-driven 渲染，代价是调试变难、compute→draw 同步纪律变严（正是 3.2/3.3 练的屏障/票据功夫的用武之地）。

四者**可叠加互不替代**。批处理条件从松到严、自由度从低到高：

```
SRP Batcher        条件: 同 shader 变体        砍: 材质状态切换
bindless(我们3.3)  条件: 同 shader             砍: 贴图绑定切换
GPU Instancing     条件: 同网格+同材质          砍: draw 数量
Instanced Indirect 条件: 同上+参数 GPU 生成     砍: draw 数量+决策权归 GPU
```

**Unity 6 的 GPU Resident Drawer 本质上是全家桶**：SRP Batcher 的标准化数据布局思想 + 常驻实例数据 + GPU 剔除 + indirect draw——数据全部 GPU 驻留，CPU 只发一条 indirect。施工计划把"GPU 剔除、常驻实例参数表与 indirect"整段划给步骤 5，就是我们的同款全家桶；**3.3 的 bindless 表是它的贴图地基**——没有"贴图按索引取"，GPU 无法自主决定用哪张贴图。

映射到项目节奏：3.4 先立裸 draw 基线（逐 primitive push，先对正确性），步骤 5 再把逐 draw 循环也砍掉（参数进表 + indirect）。

## 8. "Unity 是不是没做 texture bindless？"——三层答案

**实现了，但分三层：**

| 层 | 有没有 texture bindless | 说明 |
|---|---|---|
| 标准 SRP 提交路径 | 没有 | 逐材质贴图绑定照旧，成本被引擎内部消化，用户感知不到——SRP Batcher 只管 uniform 侧 |
| 用户 shader API | 没暴露 | HLSL 方言没有 bindless 语法；`Texture2D[]` 动态索引平台受限（DX11 SM4.0 不允许 t# 变量索引），官方不建议 |
| 引擎内部（Unity 6 起） | **有** | GPU Resident Drawer / GPU Resident Data 在支持的图形 API 上把材质数据（含贴图引用）以 bindless 形式常驻 GPU，配合 GPU 剔除 |

所以准话是：**Unity 把 texture bindless 做在了 GPU-driven 路径的内部零件里，从没作为产品能力交给用户。**不暴露的三个理由：

1. **跨平台契约**：bindless 需要 descriptor indexing 级支持，Unity 要同时伺候 GLES 3.1 / 旧 Metal / 主机 / 桌面——做成用户 API 等于把平台矩阵转嫁给所有 shader。
2. **抽象模型**：`Material.SetTexture` 语义天然映射"每 draw 绑定"，引擎按此优化了十年。
3. **边际收益的位置**：bindless 的收益只在"海量 draw + 贴图各异 + GPU 想自己做主"的场景兑现——恰好是 GPU Resident Drawer 的场景。**技术在引擎里落位的位置，跟着它兑现收益的场景走。**

（置信度标注：Unity 6 GPU Resident Drawer 使用内部 bindless 材质数据这一条，依据训练语料中的 Unity 6 文档/博客记忆，置信度较高；各图形 API 的平台支持清单未当场核验，引用前以 Unity 6 官方文档为准。）

## 9. `Texture2DArray` 对照：采样端相同，差异全在资源侧

Unity 用户能摸到的最近似物是 `Texture2DArray`——对照表恰好逐项反衬 bindless 每条特性位买到什么：

| | Unity `Texture2DArray` | 我们的 bindless 表（3.3） |
|---|---|---|
| 容量 | 建资源时定死 | 建表时定死（1024）——一样 |
| 全体成员约束 | **同尺寸、同格式**，一张 512² 一张 1024² 装不进同一个 array | 任意尺寸/格式（sRGB 与线性可混居） |
| 加一张图 | CPU 侧拷贝进空 layer，或重建更大的 array | `publish()` 原地写一个槽（UAB 授权） |
| 采样端 | `SAMPLE[tex_index]`，索引取 | `textures[tex_index]`，一样 |
| 空槽 | 无所谓（array 全量分配） | PARTIALLY_BOUND 允许存在，访问才炸 |

规律：**`Texture2DArray` 在采样端和 bindless 一模一样，差异全在资源侧**——runtime array / UAB / partially bound 每条特性位都在解除资源侧的一条约束。理解这张表，就理解了 descriptor indexing 到底买到了什么。

## 10. 待核验与 3.5 钩子

- **Unity 6 GPU Resident Drawer 平台支持清单**：本篇未当场核验（官方文档站抓取 404），引用前以 Unity 6 文档为准。
- **官方 bevy 的 bindless 材质**：印象中 bevy 0.16 起材质系统带 bindless 支持（`MaterialBind` 槽位分配、fallback 槽、不支持平台回退传统路径），设计形状与 3.3.4 的 free list + fallback 高度同构——**此条未经查证，列为 3.5"光照与同屏对照"的现成对照议题**：届时对读官方 `bevy_render` 的槽位分配账本与本仓 `vulkan/descriptors.rs`，差异入对照篇。

## 相关篇目

- [描述符索引特性族：能力位全景、三层配套与限额双轨](描述符索引特性族：能力位全景、三层配套与限额双轨.md)——特性位的规范面与三层配套。
- [3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路](3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路.md)——本篇原理的代码落地面与实测钉号。
- [纹理格式与Gamma：从png字节到采样值的色彩编码链](纹理格式与Gamma：从png字节到采样值的色彩编码链.md)——采样端解码与本篇采样端"按下标取"在 TMU 的会合点。
