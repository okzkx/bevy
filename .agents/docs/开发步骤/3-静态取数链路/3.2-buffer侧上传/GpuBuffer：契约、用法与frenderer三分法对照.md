# GpuBuffer:契约、用法与 frenderer 三分法对照

> 2026-09-24 沉淀。3.2.2.1 内存契约([施工记录](3.2.2.1-内存契约：memory type、usage、绑定与对齐.md))验收后,围绕三个问题的讨论定稿:与 frenderer buffer 三分法(RO/RW/MRW)怎么对应、Bindless 对 buffer 有什么要求、宿主可见内存在本管线里的角色。代码:`ash_renderer/src/vulkan/resources.rs`,探针:`ash_renderer/examples/memory_probe.rs`。

## §1 GpuBuffer 是什么:契约载体,不是池

`GpuBuffer` = buffer + memory + **内存契约的执行**。创建链一次把四条绑定条款全部满足(创建 → 查 requirements 三元组 → `find_type` 选型 → 恰按 `requirements.size` 分配 → offset 0 绑定),host-visible 则持久映射。它**不是**池:bump 子分配、按资产去重、容量维护归 3.2.2.2/3.2.3。

分法上,frenderer 以**类型**表达用法(底层 `RenderBuffer` 一个,用法三个,名字即同步语义);本项目以**角色**表达用法、**运行时事实**表达同步语义、类型化包装层刻意留白。覆盖等价,轴不同——理由见 §2/§3。

### 用法速查

```rust
use ash_renderer::vulkan::{BufferRole, GpuBuffer, MemoryContract, align_up};

// 契约:设备事实一次查询冻结(类型/堆表 + nonCoherentAtomSize),可长存
let contract = unsafe { MemoryContract::new(&ctx.instance, ctx.physical_device) };

// 三种角色创建:usage 与内存属性由角色表定案,创建点不重述
let staging  = GpuBuffer::create(&ctx.device, &contract, batch_bytes, BufferRole::Staging)?;
let pool     = GpuBuffer::create(&ctx.device, &contract, pool_size,  BufferRole::DevicePool)?;
let readback = GpuBuffer::create(&ctx.device, &contract, pool_size,  BufferRole::Readback)?;

// 宿主写:越界返回 Err(不 UB);非 coherent 自动按 atom 对齐 flush(写区向两端舍入)
staging.write(0, &mesh_bytes)?;
// 宿主读:非 coherent 自动先 invalidate 再拷出(验收回路用)
readback.read(region_offset, &mut out)?;
// 证据查询口:选了哪个类型、什么属性、走哪条 flush 分支
(staging.memory_type(), staging.host_coherent(), staging.allocation_size());

// 销毁纪律:Drop 不等 GPU——先以票据/fence 证明最后一次使用完成,再 drop
drop(readback); drop(pool); drop(staging);
```

规则三条:**①** 读写只认字节与偏移,越界即 `Err`,绝不越界写;**②** flush/invalidate 不由调用方管——`write`/`read` 内建,unmap 不隐式 flush 的坑没有入口;**③** 任何 GPU 在飞防护都不在这里设防,提交侧票据纪律(3.2.4)是唯一裁判。

## §2 与 frenderer 三分法对照:轴换了,覆盖等价

frender(`modules/render/vulkan/src/buffer/`)按**宿主交互方式**分三类,`RenderBuffer` 打底:

| frenderer | ash_renderer | 说明 |
|---|---|---|
| `RenderBuffer`(底层) | `GpuBuffer`(含选型/绑定/映射契约) | 骨架一致:requirements 一次查询全用上、恰量分配、offset 0 绑定 |
| `ROBuffer`(DEVICE_LOCAL,GPU 专属) | `BufferRole::DevicePool` | 同物;池形状(bump/去重)等 3.2.2.2 进驻 |
| `RWBuffer`(VIS+COHERENT,持久映射,自动可见) | `BufferRole::Staging` | 内存属性同款:HOST_VISIBLE 硬契约 + COHERENT 优先 + persistent map;差别只在 usage(纯 TRANSFER_SRC,不带 UNIFORM/STORAGE) |
| `MRWBuffer`(VIS 无 COHERENT,手动 flush/invalidate) | `GpuBuffer` 的**非 coherent 运行时分支** | 非独立类型:`host_coherent()` 决定 write 是否 flush、read 是否 invalidate,范围按 `atom_range` 从查询值取模 |
| (类型化包装层:`Vec<T>` 视图、`PhantomData`、`DescriptorSetContent`) | **留白** | 3.3/3.5 描述符与每帧 UBO 进场时再叠,且叠在 GpuBuffer 之上(不让包装层直接持有 buffer+memory) |

**RW/MRW 二分在多数硬件上是假选择。**探针实测 RTX 2060:全部三个可见类型都带 HOST_COHERENT。frenderer 的 `vk_find_memory_type` 首匹配只要求 HOST_VISIBLE,其 `MRWBuffer` 在这台卡上拿到的同样是 coherent 类型——那套手动 flush 合法但空转。类型二分表达**意图**,运行时分支表达**事实**;3.2 的验收口径(探针可证、验证层零 VUID)要求我们认事实。

**选型差异是实打实的。**`find_type` 必需项否决 + 优先位计分:readback 靠 HOST_CACHED 计分选中类型 4(+HOST_CACHED);首匹配法在 RTX 2060 上永远停在类型 3,回读路径白丢 CACHED——纯性能语义,验证层不会说话。

**代价如实记两条:**
- 调用点可读性:frenderer 的类型名即同步故事(`MRWBuffer` 一看便知"我要手动刷");我们的语义藏在角色名 + 创建日志里。
- 人体工学:元素级视图暂无。等 3.5 每帧 UBO 需要时按角色加(`PerFrameUbo`:HOST_VISIBLE 必需、票据门控复用),**不重立类型体系**。

## §3 Bindless 对 GpuBuffer 的要求:几乎无"内存面",全是"稳定与寿命"面

M2 的 Bindless 改的是**访问方式**(set0 常驻纹理/采样器表 + 索引取资源;draw 参数走 push constant),顶点/索引 buffer 在 3.4 仍走传统 `vkCmdBindVertexBuffers`,不进描述符表。因此 GpuBuffer 没有任何"为 bindless 而加"的内存位——**反面约束更明确**:池的 usage 刻意不含 `STORAGE_BUFFER`/`SHADER_DEVICE_ADDRESS`。步骤 5 的 GPU-driven 参数表到来时按"支持与启用分开"的纪律显式开位;32B 交错顶点布局不许未经检验复用为 WGSL storage struct(施工计划 §2)。

Bindless 的**驻留模型**经判定线传导过来三条,GpuBuffer 已各落一半:

1. **地址常驻**(描述符与 draw 引用稳定地址):`DevicePool` 冻结 `TRANSFER_DST`(transfer 队列异步写入,与图形提交解耦)+ `TRANSFER_SRC`(3.2.2.3 迁移路径)。
2. **寿命法则**(旧资源等最后使用结束才可销毁——与 bindless"旧槽/image view 不覆盖在飞引用"同一条):`Drop` 不等 GPU,销毁前提由 3.2.2.3 账本 + 3.2.4.4 退出排空担保。
3. **上传完成才可使用**(ticket 可证明):staging persistent map + `write()` 内建 flush,3.2.4 合批"一次一范围一提交"直接可行,不逐批重映射。

将来的新要求以**新角色**进入(角色表是唯一扩展点),不改已有三角色。

## §4 宿主侧(Host Buffer)在管线里干什么

数据通路(3.2 落地后):

```
Assets<Mesh>(CPU) → 3.1 PostUpdate 采集快照 → Last:查驻留缓存
  → 未驻留的 mesh 字节 写入 staging(宿主侧)
  → 合批一次 transfer 提交:staging → DEVICE_LOCAL 池
  → signal upload ticket(timeline)
  → 同帧图形提交:在依赖处等票据 → draw 从池里读
```

- **为什么必须有 staging:CPU 物理上写不到池。**离散卡的 DEVICE_LOCAL 类型不带 HOST_VISIBLE(探针类型 1 实证),设备内存没有宿主映射,唯一路径是宿主可见中转 + GPU 自拷。
- **Staging = 装货码头。**`write()` 末尾 flush 是"落货"(非 coherent 时宿主写不自动进设备可见域;unmap 不隐式 flush,忘了 = 提交一段驱动永远看不见的数据,所以 flush 内建、无出错入口)。批完不销毁,票据到了复用(3.2.4.3),persistent map 为此买。
- **Readback = 卸货码头,验收专用。**池在 DEVICE_LOCAL,CPU 读不到;"抽查顶点/索引内容与偏移"(3.2 验证 1)唯一路径:池 → copy 回 readback → fence → invalidate → 逐字节比对。**生产路径不经过它。**
- **为什么不把池做成 HOST_VISIBLE 省掉中转:**带宽(离散卡可见内存住 system memory/小 BAR,探针类型 5 仅 224MB);且判定线要求证合批/跨队列依赖成立,staging 是练习载体不是胶水。集成卡 DEVICE_LOCAL 自带宿主可见(类型 5),选型结果不同、契约不变——决策收进角色表的价值。

与 frenderer 对照:它把 HOST_VISIBLE buffer 当**长期数据容器**(RWBuffer 每帧同步,自注"使用要十分节制"),数据正身住宿主侧;本项目把宿主侧降级为**中转与验收的岸边**,数据正身永远驻留池中、按资产身份去重——"静态内容不因每帧快照重复上传"判定线的直接推论。

## §5 "是不是永远不用 HOST_VISIBLE"——三个说法拆开

| 说法 | 态度 |
|---|---|
| HOST_VISIBLE 作为**内存属性**(staging/readback) | 天天用:上传链两座码头 |
| HOST_VISIBLE 作为**驻留数据容器**(正身住宿主侧,渲染每帧直接读) | 刻意不用(frender er RWBuffer 模式) |
| 池的内存类型 | 只要求 DEVICE_LOCAL;设备恰发 VIS|DEVICE_LOCAL 类型是设备事实——不依赖、不 map 池 |

"永远"不成立,计划里已有一个将来形态:**3.5 每帧 UBO(set1)**。每帧 CPU 写、GPU 读、量小、复用节奏 CPU 主导——正身就该在 HOST_VISIBLE 上,住 DEVICE_LOCAL 纯浪费。届时宿主可见内存出现第三个角色;两条原则不变:复用受票据管(等本帧使用完成再写下一帧,判定线 6 的"正常同步");**静态资产永不回宿主侧**。

一句话:永远不用的是"把 HOST_VISIBLE 当驻留容器装静态资产";永远在用的是"把 HOST_VISIBLE 当码头";3.5 起再加"给每帧轮转的小数据当正身"。

## §6 边界与后续

- 本文是**设计对照篇**,不是验收记录;条款级证据(绑定四 VUID、atom 舍入细则、探针实测)在[3.2.2.1 施工记录](3.2.2.1-内存契约：memory type、usage、绑定与对齐.md)。
- 待长出的件:3.2.2.2 池(bump + 去重,复用 `find_type`/`align_up`)、3.5 每帧 UBO 角色 + 类型化宿主视图(在 GpuBuffer 之上叠,不动底座)。
- frenderer 可再借的件:元素级视图 + `DescriptorSetContent` 的一体化包装思路;其 Vec 别名方案需重审(非 coherent + 部分更新表达不了),见 §2 代价。
