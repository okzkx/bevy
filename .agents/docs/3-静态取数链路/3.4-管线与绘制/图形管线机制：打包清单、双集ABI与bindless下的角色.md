# 图形管线机制：打包清单、双集 ABI 与 bindless 下的角色

> 2026-09-29 建档（3.4 收官配套机制篇）。对应实现 `ash_renderer/src/vulkan/pipeline.rs`，
> 施工证据见[施工记录](3.4.1-3.4.5-管线与绘制施工记录：正式着色器、图形管线与DrawList同帧消费.md)。
> 讲解锚点：frenderer 的传统 set 循环（逐材质换绑）与 Unity 批处理四刀（SRP Batcher / bindless / Instancing / Indirect，见[bindless机制篇](../3.3-贴图与bindless描述符/bindless机制：从换绑到索引——frenderer锚点与Unity批处理对照.md)）。

## §0 判定线

1. **pipeline 的核心问题永远是三问**：什么东西进包、什么东西留在包外、多久编译一次。`vkCreateGraphicsPipelines` 是驱动侧大编译，draw 间切管线 = 切一整包特化状态——高到所有引擎都把 pipeline id 当 DrawList 第一排序键。
2. **bindless 之下 pipeline 的"内容"被掏空**：layout 只声明资源数组的形状，不承载"这个材质绑了什么"的具体拓扑；"选哪张"搬进数据（push 里的两个 u32 索引）。资源选择权从绑定状态搬进索引数据——与 bindless 机制篇判定线同一条。
3. **pipeline layout 是所有管线与描述符表之间的稳定 ABI**：set0/set1 layout 从 BindlessTables 取、不另建——同源即可绑，未来变体管线"换管线不动表"。
4. **表在飞更新，管线无感**：UAB 写槽不经过 pipeline 对象；"加贴图"与"重建管线"是两条完全独立的生命周期。
5. **诚实边界**：M2 管线数 = 1，"变体共享布局"尚未被第二条管线检验；逐 draw push 仍是 CPU 每对象装配——步骤 5 迁常驻实例表 + indirect 时被砍的就是这一刀。

## §1 pipeline 对象：一次昂贵的编译期打包

Vulkan 的 graphics pipeline 把 shader 入口 + 全部固定功能状态打包成一个驱动特化过的对象。换管线 = 换一整包状态，代价高到批处理的第一规则就是"按 pipeline 排序，少切"。所以设计 pipeline 永远在答三问：进包什么、包外什么、多久编一次。

## §2 pipeline.rs 的答案

| 三问 | 我们的答案 | 依据 |
|---|---|---|
| 进包什么 | `debug_draw` 双入口（vs_main/fs_main 同模块）+ 顶点输入（32B 交错，与 `mesh_convert` 同契约）+ reverse-Z 三件套（GREATER/清 0/无限反向投影）+ cull NONE + blend 关 | 每项都是定案不是默认：cull NONE 是 M2 材质策略（HoseMat 双面 + 镜片不透明覆盖 + 正高度下绕向未镜像） |
| 留在包外什么 | viewport/scissor（动态状态）| 重建成本转移到每帧两条 cmd，买 resize 零管线重建 |
| 多久编一次 | 进程级一次（与 Context 同层） | M2 一条管线画一切，没有变体就没有重建理由 |

创建时机排在初始化链**最末**：它吃两样别人的产出——swapchain 的 `view_format`（SRGB 别名）+ `DEPTH_FORMAT`（`PipelineRenderingCreateInfo` 内联声明附件格式，dynamic rendering 无 RenderPass 兼容链），以及 BindlessTables 的两个 set layout。对附件格式是**值依赖**，对描述符集只有**句柄借用**——拆除序里它死在 BindlessTables 之前。shader module 经 `include_bytes!` 消费 build.rs 写进 OUT_DIR 的补丁产物，module 只是编译输入、建完即毁。

## §3 特别之一：绑定次数从"每材质"降到"每帧一次"

`record_frame` 里 `cmd_bind_descriptor_sets` 只出现一次（set0 常驻集 + 本帧槽 set1），之后六个 draw 只做 `cmd_push_constants`（96B：model/tex_index/sampler_index/base_color）+ `cmd_draw_indexed`。对照传统路径（frenderer set 循环）：每材质一次 set 换绑或改写。按四刀框架：**PSO 换绑这一刀**被"一条 shader 画一切 + 稳定布局"砍掉，**贴图换绑这一刀**被 bindless 索引砍掉。

Unity 对照：push-constant 驱动的材质变化与 SRP Batcher 的"同 shader 变体只更新数据不换管线"同构；而贴图走索引更进一步——标准路径里换贴图终归是一次绑定变化，这里连绑定都不碰。

## §4 特别之二：表在飞更新，管线无感

UAB 三层配套里，管线只接触 layout 这一层（`UPDATE_AFTER_BIND_POOL` + binding 旗标 UAB+PARTIALLY_BOUND）。运行中 `BindlessTables::publish` 往 set0 写新槽，**已绑定的集当场生效，加贴图全程不经过 pipeline 对象**——"加资产"与"重建管线"是两条独立生命周期。PARTIALLY_BOUND 保证 partially 未写的空槽不要求合法描述符，fallback 槽 0 兜住未发布下标（空槽不可访问的 UB 由"只写新槽 + fallback 缺省"的结构纪律避开）。

一个容易漏看的契约：WGSL 里是**无长度** runtime array（shader 视角无界，`OpTypeRuntimeArray`，build.rs 补 capability），layout 里 `descriptor_count(1024)` 才是容量真身——"无界"是着色器的观感，"容量"是布局的账本，靠 VUID 对上；改容量必须回 set/binding 表文档对账。

## §5 特别之三：layout 成为未来所有管线的公共 ABI

管线与 set 的兼容规则是"layout 同源即可绑"。因为两个 set layout 都从 BindlessTables 取、不另建，**3.5 的 unlit vs Lambert 变体只要复用同一对 layout，换管线就是一次 `cmd_bind_pipeline`，set0/set1 绑定原样有效**——DrawList 排序键里 pipeline 第一优先，这个设计让"排序换管线"的成本降到最低。执法点也在管线创建：带 runtime array capability 的 shader 只在五个 feature 位已启用的设备上建得出管线（VUID-08740 capability↔feature，3.3 钉号 VUID-03252 特性集须覆盖全部消费者）——**vkCreateShaderModule/Pipeline 是 capability 的执法现场**，这就是 context.rs 五位硬启用存在的理由。

stage flags 取 COMPUTE|VERTEX|FRAGMENT 超集（descriptors.rs）：超集声明只影响驱动的描述符优化面，不改变合法性——3.3 compute 探针与 3.4 graphics 管线共用同一 layout 的合法通道。

## §6 定位与下一步

四刀框架下本管线已经砍掉两刀（PSO 换绑、贴图换绑），剩下的逐 draw CPU 装配（push + draw call）是步骤 5 迁常驻实例表 + indirect 的对象——**现在的 record_frame 就是那时的 A 侧对照**。M2 管线数 = 1："变体共享布局"的承诺要等 3.5 第一条分岔管线兑现检验；DrawList 排序键（pipeline 优先）在管线数 >1 之前也不会真正起作用。
