# GPU驱动的第一课：从"CPU逐个喂参数"到"GPU自己查表"——5.1新旧两套写法对照讲解

> 2026-10-15 收官配套讲解篇，面向第一次接触 GPU-driven 渲染的读者（即你）。全部代码摘自本仓库 5.1 前后的真实源码，旧代码标注"已退役"。读完你应该能回答四个问题：旧写法每帧在浪费什么、新写法把每一样数据搬到了哪、"写形状"和"环"为什么长那样、5.2/5.3 接下来吃什么。机制先行，比喻只做对照；Unity/UE 类比集中在 §8。
> 配套：[施工记录](5.1.1-5.1.6-常驻绘制数据施工记录：对象表、模型环与push退役.md)（判定线证据）、[流程导读](GPU常驻绘制数据流程：从账本行到实例索引的各级账面.md)（账面全景）。

## §0 先立三个核心判断（全文反复回来看）

1. **GPU-driven 的定义不是"用了 GPU"，而是"CPU 不逐对象参与绘制决策"**。旧写法里 GPU 每画一个物体都要 CPU 亲口告诉它参数；新写法 CPU 只递一个编号，参数本体住在 GPU 里，GPU 自己去查。
2. **数据按"写形状"分家，是这一段唯一重要的设计决策**：一份 GPU 数据怎么被安全地更新，取决于它多久变一次——"基本不变"和"每帧都变"必须用两套完全不同的机制，混用要么性能崩、要么是未定义行为。
3. **"环"不是新发明，你早就写过**：帧 UBO（相机/灯光）和 UI 顶点环就是环。5.1 只是把同样的纪律推广到逐对象数据。

---

## 一、旧世界：CPU 逐个喂参数（已退役的写法）

### 1.1 数据在哪里

4.x 收官时，每个可渲染物体在账本里有一行（`InstanceRow`）：

```rust
// 4.x（已退役）——行内容就是实例数据终值，但只活在 CPU 侧
pub struct InstanceRow {
    pub mesh: Handle<Mesh>,        // 画哪个网格
    pub material: Handle<StandardMaterial>, // 用什么材质
    pub model: Mat4,               // 它在哪（local→world 矩阵）
}
```

GPU 侧**没有任何一份"场景里有哪些物体"的账**。GPU 知道的只有：顶点池（ mesh 的几何字节）、bindless 纹理表、每帧一份 128B 的相机/灯光 UBO。物体本身的参数——矩阵、用哪张贴图、什么颜色——每一帧、每一个物体，都由 CPU 亲手塞进 **push constant**。

### 1.2 push constant 是什么，为什么它成了瓶颈

push constant 是 Vulkan 里一种"跟着命令走的小参数"：录制命令时把字节直接写进命令流，GPU 执行到这条 draw 时就地能读。它快（无绑定开销），但有个硬约束——**总大小有上限（典型 128~256B），而且它是命令流的一部分：一个物体一份，一个都不能少**。

我们旧管线每个物体推 96B：

```rust
// 旧 pipeline.rs（已退役）
pub struct PushData {
    pub model: [f32; 16],      // 64B：local→world 矩阵
    pub tex_index: u32,        // 贴图在 bindless 表里的下标
    pub sampler_index: u32,    // 采样器下标
    pub base_color: [f32; 4],  // 材质基色
}
// pack_push() 把它手工排成 96B 字节，每个 draw 之前推一次
```

录制端逐物体循环：

```rust
// 旧 frames.rs（已退役）——CPU 绘制调用数 = 物体数，push 字节 = 96 × 物体数
for call in draw.draws {
    let push_bytes = pack_push(&call.push);   // CPU 每物体打包 96B
    device.cmd_push_constants(..., &push_bytes);
    device.cmd_draw_indexed(..., call.index_count, 1, call.first_index, ...);
}
```

而这份 `call.push` 是宿主每帧现做的——draw_frame 走访账本**全部行**，逐行查驻留、查材质容器、组装 96B：

```rust
// 旧 host/frame.rs（已退役）——每帧 O(N) CPU 组装
for (_entity, row) in ledger.rows() {
    let slot = pool.resident(row.mesh.id())?;      // 现查网格在显存哪
    let material = std_materials.get(&row.material)?; // 现查材质容器
    draws.push(DrawCall {
        push: PushData {
            model: row.model.to_cols_array(),      // 矩阵搬进 push
            tex_index: slots.texture,              // 贴图槽搬进 push
            base_color: [color.red, ...],          // 颜色搬进 push
        },
        vertex_base: slot.vertex_base(), ...
    });
}
```

### 1.3 这套写法的三条天花板

| # | 天花板 | 具体机制 |
|---|---|---|
| ① | **CPU 每帧成本随物体数线性涨** | DrawList 组装 O(N) + 96B×N 的 push 字节 + N 次录制调用。4.1 把"上传"做成了增量，但"组装绘制"还是每帧全量——物体越多，CPU 每帧越重 |
| ② | **GPU 是哑的** | 参数全在 CPU 手里，GPU 只能"给什么画什么"。想让它自己剔除不可见的物体？它连"场景里有哪些物体"都不知道——没有表可查 |
| ③ | 绘制调用数 = 物体数 | N 个物体 = N 条 `cmd_draw_indexed`。10 万物体 = 10 万条绘制命令，光命令处理就是负担 |

**这三条天花板就是"GPU-driven"要拆的东西**：GPU 自己拿数据（修②）→ 自己决定画谁（5.2 剔除）→ 自己写绘制命令（5.3 indirect）。5.1 干的是第一件：**把数据搬过去**。

---

## 二、第一刀：把参数搬进 GPU——对象表与实例索引

### 2.1 对象表：一张"物体名册"

新写法在 DEVICE_LOCAL 显存里立一张表，一个物体一行（我们叫**实例槽**）：

```wgsl
// debug_draw.wgsl——GPU 视角的对象表（set2 b0，只读 storage）
struct ObjectRecord {
    first_index: u32,      // 索引池里这个 mesh 从哪开始（画的范围）
    index_count: u32,
    vertex_base: u32,      // 顶点池里的起始顶点号
    material_index: u32,   // 材质环的下标（见 §四）
    aabb_min: vec4f,       // 局部包围盒（5.2 剔除要用，5.1 先入库）
    aabb_max: vec4f,
}
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
```

shader 拿到编号就自己查：

```wgsl
// push 从 96B 缩到 4B：只剩一个编号
struct PushParams { instance: u32 }
var<immediate> push: PushParams;

@vertex
fn vs_main(@location(0) position: vec3f, ...) -> VsOut {
    let obj = objects[push.instance];      // ← GPU 自己查表！
    let model = models[push.instance];     // ← 矩阵也从表里拿（§四讲为什么单独一张表）
    out.clip = frame.view_proj * (model * vec4f(position, 1.0));
    out.material = obj.material_index;     // 材质编号传给 fragment
    ...
}
```

CPU 侧从"组装 96B"变成"递一个编号"：

```rust
// 新 frames.rs——每物体 push 4B
for call in draw.draws {
    let push_bytes = call.instance.to_le_bytes();   // 4B，就一个槽号
    device.cmd_push_constants(..., &push_bytes);
    device.cmd_draw_indexed(...);                    // 其余照旧
}
```

这就是判定线"push 字节数与对象数解耦"的**结构**含义：每个 draw 推的东西从"一整个物体的参数"退化成"一个下标"。参数本体（矩阵/贴图/颜色）零推送。

### 2.2 为什么叫"槽"而不是"行号"

槽（slot）是**跨帧稳定的编号**：物体 A 入场时分配 7 号，只要它活着就一直用 7 号；它删除后 7 号进回收流程，将来新物体可能复用 7 号。这和 4.1 的"稳定槽账本"是同一个思想往 GPU 侧延伸——账本行记 `instance_slot: Option<u32>`，`None` 表示还没驻留成表（不可画）。

为什么不直接用"第几个物体"当下标？因为物体每帧增删，如果用行序当下标，一个物体删除会让后面所有物体编号偏移 = 全表重写。**稳定槽 = 增量的前提**（4.1 教过的课，在第三层账上再上一遍）。

---

## 三、"写形状"：为什么 model 不能和范围参数住同一张表

这是本段最重要的设计课。假设我们把 48B 的对象记录改成"范围 + 材质槽 + **model 矩阵**"一张表全装了——看起来更简单，实际会撞上一个 Vulkan 的**规范级数据竞争**：

### 3.1 想象一次移动

物体 X 在 7 号槽。用户拖动它，flush 需要把新矩阵写进 7 号槽（`vkCmdCopyBuffer`，transfer 队列）。**同一时刻**，上一帧的 draw（GPU 还在光栅化）正在读 7 号记录里的旧矩阵。

一次是写、一次是读、两边没有同步依赖——这在 Vulkan 规范里是 **data race → 未定义行为**。不是"可能闪一帧"的良性竞态，是 UB；同步验证层（synchronization validation）会把它当 buffer hazard 抓出来，我们的"零 VUID"判定线直接爆。

（对齐记忆：4.2/4.4 处理"同柄内容重传"时用 swap-on-commit——**新槽写新内容，旧槽进停尸房**——躲的就是同一个问题。）

### 3.2 swap-on-commit 救得了材质，救不了移动

换绑/热重载走"新槽 + 旧槽停尸房"没问题，因为**稀疏**。但移动是高频操作：10 万物体里 1 万个每帧都在动，每个动的物体每帧都要"新槽 + 旧槽进停尸房"。停尸房的排空条件是"等全部帧槽的 fence"（证明没有在飞 draw 还在读）——free list 永远是空的（刚排空又塞满），于是**每帧 flush 都在等 GPU 把帧跑完 = 流水线全停**。异步上传机器直接报废。

### 3.3 落定：按"写形状"分三张表

数据按**变化频率**（= 写形状）拆开，每块用配得上它的安全机制：

| 表 | 内容 | 变化频率 | 写法 | 在飞安全凭什么 |
|---|---|---|---|---|
| **对象表** | mesh 范围、材质槽号、局部包围盒 | 基本不变（只有入场/换绑/换网格才变） | **每槽写一次**；更新 = 新槽 + 旧槽停尸房 | 读侧：图形提交等上传票据；写侧：复用槽必先经停尸房排空（等全部帧槽 fence） |
| **模型环** | model 矩阵 | 每帧都可能变（动了才写） | draw_frame 增量重写：只写"这一帧里变过的行" | `wait_for_slot` 的帧槽 fence——与本帧 UBO 同一条证据链 |
| **材质环** | base_color、贴图槽号 | 少量变 | draw_frame 每帧**全量**重写（量级 = 可绘材质数，头盔才 5 个） | 同上 |

理解的关键：

- **对象表不是"每帧更新的表"，是"几乎不更新的表"**——所以它敢放 DEVICE_LOCAL（GPU 独享显存，最快），也敢承诺"这个槽的内容永远不用改"（要改就换槽）。shader 读它零竞态。
- **model 单独拆出来**，不是因为矩阵特殊，是因为它属于"移动 = 改内容"的一类。易变数据的家不是常驻表，是**环**（§四）。
- **材质环选"全量重写"是个杠杆**：材质内容本身会变（mutate、贴图热重载），如果材质记录也走"写一次 + 换槽"，那么 10 万个实例共享 5 个材质时，改一张贴图 = 5 个材质换槽 = **10 万个对象记录跟着换槽**（它们记着材质槽号）——链式 churn。全量重写把这条链砍断：贴图变了，下一帧材质环里那 5 条记录自然就是新值，10 万个对象记录**一个都不用动**（它们记的槽号没变）。代价是每帧重写"材质数 × 32B"——几百字节，忽略不计。

一句话总结写形状：**"写一次"的数据最贵也最安全，"增量写"的数据要自带两份轮转，"全量写"的数据必须足够小。**三类数据放错位置，要么撞 hazard，要么白搬。

---

## 四、"环"：易变数据的家（你其实早就写过）

### 4.1 环解决的问题只有一个——而且 barrier 管不了它

一块 host-visible buffer，CPU 每帧写、GPU 每帧读。**CPU 写第 N 帧时，GPU 可能还在执行第 N-1 帧的 draw（它正在读同一块 buffer）**——直接写 = 又是 hazard。

先排除一个常见误解：**"为什么不用 barrier，在绘制完之后再执行修改 buffer 的命令？"**——因为 barrier 是录进命令缓冲的指令，它排序的是 **GPU 命令之间**的访问（布局迁移、同提交内写后读、跨提交配信号量的交接，这些岗位我们一直在用屏障和票据信号量）。而环要处理的写是**宿主映射内存的直接写**（`vkMapMemory` 的指针 `memcpy`）——它根本不是一条 queue 命令，barrier 排不了它的队；规范对 mapped memory 的纪律就是"宿主访问不得与在飞 GPU 访问并发"，这句话没有命令侧的执法点，只能靠 **CPU 侧选时机**。

选时机的武器 = fence：**CPU 等到"上一轮使用者已经跑完"再写**。环 ×N 份轮转给这个等待留出空间——只有一个 buffer 时，CPU 每帧都得等 GPU 全跑完（= MAX_FRAMES_IN_FLIGHT 退化成 1，异步流水线掐死）；×2 份后，CPU 写 0 号环时 0 号环的旧读者已被 fence 证明退场，GPU 正在读的是 1 号环。**环不是 barrier 的替代品，环 + fence 是宿主写的组合拳**：环解决物理重叠，fence 证明时机安全。

解法：备 **MAX_FRAMES_IN_FLIGHT 份**（我们 = 2），按帧槽轮转。帧 N 用 0 号环，帧 N+1 用 1 号环，帧 N+2 又轮回 0 号环。**CPU 敢写 0 号环的前提，是帧 N 的提交已经跑完了**——这正是 `wait_for_slot` 里那个 fence 等的事情：

```
帧 N-2 用 0 号环 → 帧首 wait_for_slot 等它的 fence → CPU 确认 GPU 读完了
→ 现在 CPU 写 0 号环才安全 → 帧循环里 write UBO / 写模型环 / 写材质环全在这之后
```

### 4.2 三个"环"，同一个纪律

```rust
// draw_frame 里三兄弟排在一起——同一套 fence 纪律
tables.frame_ubo(frames.current_index())          // 帧 UBO：相机/灯光（3.4 就有）
    .write(0, &pack_frame_uniforms(...))?;
drawlist::write_rings(instance_tables, ledger, ...) // 模型环 + 材质环（5.1 新增）
    .or_fatal("模型/材质环写失败，渲染链无法继续")?;
```

帧 UBO 是"每帧全量重写"的环（128B 全量），UI 顶点环是"每帧整换"的环，模型环是"每帧**增量**"的环——写多少不同，安全机制完全一样。**环 = 按帧槽轮转的多份拷贝 + fence 保证写时无人读**。

### 4.3 模型环的增量写：为什么是"两个帧槽各写一次"

增量写有个隐蔽问题：帧 N 改了物体 A 的矩阵，但 0 号环上 A 的旧矩阵是帧 N-2 写进去的——**1 号环还躺着旧的**。所以账本里立了**两套待写标记**（按帧槽奇偶）：

```rust
// ledger.rs：改模型时，两个帧槽的环都标记"欠一次写入"
pub(crate) fn mark_model_dirty(&mut self, entity: Entity) {
    self.model_dirty[0].insert(entity);   // 0 号环欠写
    self.model_dirty[1].insert(entity);   // 1 号环欠写
}
// draw_frame(frame) 只取自己奇偶的那套，写完清掉；
// 另一套留给下一帧——两帧之后，两个环都新鲜
```

代价：每个改动行每帧写 64B（两个环交替），**静态场景零写入**。对比旧写法：每帧每物体 96B push——动态物体省在"只写改动的"，静态物体的参数彻底离开 CPU。

### 4.3 材质环为什么敢全量重写

材质记录的量级是**唯一材质数**（不是实例数！）——FlightHelmet 六个 primitive 共享 5 个材质，重写 5 条 × 32B = 160B。而且每帧从资产容器现读意味着：材质参数 mutate、贴图槽换指，**下一帧自动生效，不需要任何失效通道**。4.2 立的"材质没有 GPU 驻留内容"这句话在新世界里依然成立——材质环本质上是"每帧把材质参数当全局量重报一遍"。

---

## 五、槽位的生与死：停尸房（你已经见过两次了）

实例槽是 GPU 资源，删除不能立刻复用——在飞 draw 可能还在读旧记录。这套纪律 4.4 在网格区间上建过（MeshPool 停尸房），对象表照抄同一套：

```
despawn → update_ledger 清行，旧槽进 ledger.freed_slots（纯整数账，账本零 Vulkan）
        → flush 开头把 freed_slots 送 InstanceTables 停尸房（仍不可复用）
        → 下帧 flush 开头 drain_graveyard：
              等"图形最后使用"（全部帧槽 fence = 没有任何 draw 还在跑）
              → 归还 free list → 之后 alloc 才可能拿到它
```

**"删了就复用"为什么是 bug**：删除那一帧的上一帧提交还在 GPU 上跑，它的 DrawList 里还有这个物体（删除是这帧才发现的），它正在读 7 号记录。此刻复用 7 号 = 覆盖在飞 draw 正在读的内存。旧内容**多活一帧**是安全的价格，与网格区间/旧贴图完全同款。

材质槽例外：**bump-only 永不回收**。材质数量级小（唯一材质数），回收要跟踪"还有没有行引用它"，维护成本买不回那几十个槽——泄漏有界（65536 上界，超限报错）。这是工程判断：不是所有资源都值得建完整的回收链。

---

## 六、新世界全流程：一个物体从 spawn 到上屏

```
[Update]          BRP spawn：实体入场
[PostUpdate]      update_ledger（传播后）：
                    行入账 {mesh柄, material柄, model终值, slot:None, bound:None}
                    dirty 置位
[Last·flush_uploads]
  0. 三家停尸房排空（网格区间/旧图/旧实例槽，共用一次"等全部帧槽 fence"）
  1. 脏行盘点：mesh 驻留? 材质五槽驻留? （4.x 语义原样）
  2. mesh 腿完成 →
       ensure_material_slot（材质槽 bump，跨帧稳定）
       alloc_object_slot（free list 优先，否则 bump 游标）
       记录身份比对 (mesh_id, ticket, material_id)——变了才换槽
       48B 记录排进本批 staging → vkCmdCopyBuffer → 对象表
       旧槽(若有)进停尸房
       mark_model_dirty（两套奇偶）
  3. 提交 transfer 批 → 票据 T
[Last·draw_frame]
  4. wait_for_slot（帧槽 fence）
  5. acquire
  6. DrawList 槽位枚举：行 → {instance: slot, 池引脚}   ← CPU 只递编号
  7. 帧 UBO 写 + 模型环增量写 + 材质环全量写（同一条 fence 纪律之下）
  8. 录制：绑 set0/1/2 → 每 draw push 4B → cmd_draw_indexed
     （图形提交在 GPU 侧等票据 T——"对象表写完了才可画"）
[present]
```

shader 侧（新世界的完整消费端）：

```wgsl
@vertex
fn vs_main(@location(0) position: vec3f, @location(1) normal: vec3f, @location(2) uv: vec2f) -> VsOut {
    let obj = objects[push.instance];    // 范围/材质槽/包围盒
    let model = models[push.instance];   // 矩阵从模型环来
    out.clip = frame.view_proj * (model * vec4f(position, 1.0));
    out.world_normal = normal_cofactor(model) * normal;
    out.material = obj.material_index;   // flat：编号不是几何量，不插值
    out.uv = uv;
    return out;
}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let mat = materials[in.material];    // 颜色/贴图槽从材质环来
    let albedo = textureSampleLevel(textures[mat.tex_index], samplers[mat.sampler_index], in.uv, 0.0)
                 * mat.base_color;
    ... // Lambert/Unlit/Normal 三模式照旧
}
```

一个值得钉住的细节：`objects[push.instance]` 用 push constant 当下标——push 在一个 draw 内是常量，GPU 不需要 NonUniform 修饰（那是"同一 draw 内每线程下标不同"才需要的）。

---

## 七、新旧逐项对照总表

| 事项 | 旧写法（≤4.x） | 新写法（5.1 起） | 差在哪 |
|---|---|---|---|
| model 矩阵 | 每 draw push 64B | 模型环 64B/槽，动了才写 | 静态物体矩阵永远不过 CPU |
| 材质参数/贴图槽 | 每 draw push 12B，每帧从容器现读 | 材质环 32B/材质，每帧全量重写 | 参数与物体数解耦（×材质数） |
| mesh 范围 | CPU 侧 DrawCall 字段 | 对象表记录（副本）+ CPU 引脚并存 | GPU 可自查（5.3 indirect 的前提） |
| draw 命令 | N 条 cmd_draw_indexed + N 次 push 96B | N 条 + N 次 push 4B | **命令数没变**（5.3 indirect 才动它） |
| DrawList 组装 | O(N) 组装 96B 参数 | O(N) 查槽号（组装参数职责已死） | CPU 每物体工作量：96B 打包 → 4B 查号 |
| 场景全景表 | 无 | 对象表（GPU 可查） | **5.2 剔除的存在前提** |
| 移动物体成本 | 96B push（每帧每个 draw） | 64B 环写（只写改动的） | 静态场景趋零 |
| 删除/复用 | 无槽概念 | 停尸房 + free list | 第三层账（资产/实体/实例槽） |
| 包围体 | 无 | 局部 AABB 入库（本段）+ 保守变换纯函数 | 5.2 compute 剔除的粮食 |

**诚实的边界**（别高估 5.1）：CPU 仍每帧走访全部账本行（槽位枚举 O(N)），绘制命令仍逐物体录制。5.1 拆掉的是"逐对象参数的 CPU 推送"；"逐对象命令的 CPU 录制"归 5.3 indirect，"逐对象可见性判断"归 5.2 compute。三刀砍完才是完整的 GPU-driven。

---

## 八、Unity/UE 对照（你的母语参照系）

| 本工程 | Unity | UE |
|---|---|---|
| 对象表（范围+材质槽+包围盒，GPU 可查） | GPU Resident Drawer 的 GPU instance data（Unity 6 内部，用户 API 不可见） | mesh pass 的 GPU scene 数据 + primitive fuzzy mapping |
| 模型环（每帧增量写易变矩阵） | 传统路径 per-object `unity_ObjectToWorld` cbuffer（SRP Batcher 合批的那份）——本质也是环式 cbuffer | FPrimitiveSceneProxy 每帧 upload 的 primitive uniform |
| 材质环（每帧重写小表） | SRP Batcher 的 per-material cbuffer 思想（材质数据与物体数据分列） | material uniform buffer 集 |
| push 4B 实例槽号 | SRP Batcher 的 `unity_InstanceID`/instance offset——同一刀：CPU 递索引不递数据 | indirect draw 的 DrawIndexedArgs，参数在 GPU |
| 停尸房 | 未必暴露（引擎内部 JobSystem 时序管理） | RHI 的 deferred delete 队列（等 fence 再真删） |

你之前问过的 Unity bindless 三层拆解里有一条"标准路径无 bindless"——5.1 之后可以再加一句：**Unity 6 的 GPU Resident Drawer 就是我们 5.1+5.2+5.3 三段合起来的官方版**：instance 数据常驻（5.1）+ GPU 剔除（5.2）+ indirect（5.3），Unity 只是把三刀一起卖给想省 CPU 的用户。

---

## 九、下一步：这张表怎么被 5.2/5.3 吃掉

- **5.2 GPU frustum**：一个 compute shader 读同一张 set2（layout 的 stage 已声明 COMPUTE——现在就写着），对每个实例槽：拿 `models[slot]` × `objects[slot].aabb` 算世界包围盒 → 与视锥求交 → 写一个可见标记 buffer。判定线红线：CPU **不回读**这个标记——它只给 5.3 用。你已经在库里见过的 `conservative_world_aabb`（8 角点重收紧）就是 CPU 侧的参照实现。
- **5.3 indirect 闭环**：compute 把可见实例的 `index_count/first_index/vertex_base/instance` 写进 `VkDrawIndexedIndirectCommand` 数组，CPU 一次 `vkCmdDrawIndexedIndirectCount` 画完——**N 条 cmd_draw_indexed 变 1 条**，本表"每槽写一次"的不变式让 compute 可以安全地读取全部记录。
- 到那时再回头看：对象表=舞台名单，模型环=演员实时走位，材质环=妆容台，5.2=导演划掉不上场的，5.3=节目单交给 GPU 自己念。名字会忘，"谁负责写、多久写一次、凭什么安全"这三问不会。

## 十、自测四问（复述用）

1. 旧管线画 100 个物体，每帧 push 多少字节？新管线呢？（9600B vs 400B；且新管线的 400B 是索引不是参数）
2. 为什么 model 不能住对象表？说出 hazard 的两端。（transfer 队列在写 vs 图形队列在飞 draw 在读，无同步 = UB；swap-on-commit 为什么救不了：排空等待会把流水线打成每帧全停）
3. 模型环为什么要 ×2？如果 MAX_FRAMES_IN_FLIGHT=3 要几份？（帧槽轮转写时必须无人读 → 每个在飞帧一份，3 份；待写标记也要三套奇偶——这套设计里"帧槽数"是环和脏集的维度）
4. 删物体后它的槽为什么不能马上给新物体用？（在飞 draw 的 DrawList 里还有它；等"图形最后使用"=全部帧槽 fence 后才归还）
