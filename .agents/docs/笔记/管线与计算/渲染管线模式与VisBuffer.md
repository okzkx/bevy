# 渲染管线模式与 VisBuffer：像素上的光照信息从哪来

《GPU-Driven 本质》走完的那条链回答的是“谁在什么时候决定画什么”：CPU 每帧只写相机参数、可画行列表（行 = 对象表里的槽位）和一条 indirect 绘制命令（绘制参数写在显存缓冲里、GPU 读缓冲执行 draw 的命令），GPU 的 compute shader（在 GPU 上运行的通用计算程序）算出可见性与 LOD，图形阶段消费命令。那篇停在“画哪些对象”。对象确定之后，每个像素上的颜色怎么算，是另一半问题。

一个不透明像素要着色，需要知道它的法线、反照率、粗糙度/金属度、被哪些灯照到、有没有阴影。这些信息在什么时候算、算完存在哪，就是着色数据组织层要做的决策。它和 GDR 的决策层各算各的成本：

| 决策层 | 回答的问题 | 成本主要跟什么走 |
|---|---|---|
| 决策与提交层（GPU-driven rendering，GDR） | 画哪些对象、用哪档网格、命令怎么生成 | 对象数 |
| 着色数据组织层（forward / deferred） | 每个像素的材质与光照信息什么时候算、存成什么格式 | 光源数与材质带宽 |

所以两个决策可以分开选择：场景对象很多但只有一盏方向光，GDR 值得上，deferred 未必；灯很多但对象很少，deferred 值得上，GDR 不值得。

延迟渲染（deferred）把不透明物体的材质参数先写进一组与屏幕同尺寸的中间图，叫 G-buffer（几何/材质缓冲）；光照 pass 再读这组图逐像素算颜色。vis-buffer（可见性缓冲）是同一位置上的另一种中间图：每像素只记被哪个三角形盖住、在三角形内什么位置，属性留到着色时再回表算。

> **两个决策正交；vis-buffer 不是“额外加一个 buffer”，而是 G-buffer 同一职责位的另一种格式（存三角形 ID+重心坐标，不存属性结果），二选一，不并存。**

行业里两种管线都在和 GDR 组合。下面先看组合谱系，再拆 vis-buffer 的机制和它抢的职责位，最后看本仓库步骤 8 之后落在哪一格。

## 一、组合谱系：两种管线模式都在用 GDR

几何簇（cluster）是把网格切成的一组小三角形批次，GPU 可以单独剔除、单独光栅化；微三角形是屏幕覆盖不足一个像素的三角形。

| 引擎/案例 | 着色管线 | GDR 形态 |
|---|---|---|
| UE5 Nanite | deferred | indirect + 几何簇（cluster）微三角形 + vis-buffer |
| Frostbite（GDC 2016 奠基分享） | deferred | compute 剔除 → indirect |
| Ubisoft（SIGGRAPH 2015） | deferred lighting | GPU 剔除发 indirect |
| Unity 6 GPU Resident Drawer | URP=Forward+（官方唯一支持路径） | 实例数据常驻 + GPU 剔除，需手动开启 |
| DOOM 系列（id Tech） | forward（clustered） | 预生成的网格绘制命令（mesh draw command）+ GPU 剔除 |
| COD 系 | forward+（clustered） | GPU 剔除、indirect |

大世界 3A 多落 deferred 一侧（历史惯性 + 光源多），但这不是 GDR 的要求——id Tech 和 COD 证明 forward+ 照样跑全套 GPU 剔除和 indirect。Unity 6 的 GPU Resident Drawer 只在 URP 的 Forward+ 路径启用，正好从反面说明：GDR 不必配 deferred。

管线模式由光源数、材质复杂度、MSAA 与透明占比决定：

- 光源多：deferred 在全屏光照 pass 里每个像素只算一次，不会随物体数乘灯数；forward（每画一个物体，当场把它的材质和光照算成颜色）每画一个物体都要遍历灯，灯多时成本涨得快。forward+（clustered forward）先把屏幕切成格子、筛出每格光源，前向着色只遍历本格列表，补的是 forward 的多光源短板。
- 材质复杂、带宽紧：deferred 只为可见像素算材质，forward 可能为被遮挡的像素多算一遍；但 deferred 要把材质参数写进 G-buffer，带宽开销另记。
- 要 MSAA（多重采样抗锯齿：每个像素存多个样本再平均）：deferred 的 G-buffer 要按样本存多份，带宽代价高，所以延迟管线通常配后处理抗锯齿；forward 可以直接用硬件 MSAA。
- 透明占比高：透明必须逐层混合，deferred 的先收集再统一着色用不上，透明越多，deferred 的收益越小。

GDR 的选择则只看对象数：对象少时，CPU 逐对象剔除、装配参数、发 draw 更便宜；对象多到 CPU 循环成为瓶颈，常驻表与 indirect 才划算。

## 二、与 GDR 真正绑定的只有两件事

如果只看组合谱系，容易以为 GDR 和 deferred 是一对。真正和 GDR 绑定的只有下面两件事，都不是 forward/deferred 之分。

### 不透明主通道才进 indirect

indirect 的命令缓冲里放的是不透明几何的绘制命令；这些几何能写深度。半透明要逐层混合、要读改写颜色，不能和不透明物体共用同一条“一次画完”的命令缓冲；它永远另走独立的 forward pass。所以任何 GDR 引擎实际都是混合形态：不透明 GPU-driven、半透明传统 forward。这与选 deferred 还是 forward 无关——deferred 引擎的半透明同样是独立 forward pass，借不透明留下的深度做深度测试。

本仓库的具体形态：`build_indirect` 给透明行写的 `instanceCount` 是 0；透明行由 `transparent_collect` 紧凑、`transparent_sort` 排序、`transparent_gather` 另建一条间接命令缓冲（每条命令 20B），透明前向一次 `cmd_draw_indexed_indirect` 画完。两条命令缓冲、两个 pass，互不混。

### 最极致的 GDR 引擎倒逼着色层进化

Nanite、id Tech、Frostbite 都长成 vis-buffer / deferred material（几何只写可见性信息、材质留到 resolve 阶段再算）形状。因果不是“GDR 必须用它”，而是：对象数多到 GDR 成为必选项后，几何被切成可单独剔除、单独光栅化的几何簇，三角形越来越碎，直到大量微三角形。这时“光栅化时给每个三角形插值整套属性再写 G-buffer”的成本会随三角形数爆炸，vis-buffer 才成为自然选择。方向是：对象数 → GDR → 几何碎片化 → vis-buffer。

## 三、VisBuffer 的机制：每像素只存索引

vis-buffer 是一张与屏幕同尺寸的中间图。每像素只记两样东西：

- 盖住它的三角形 ID，以及它属于哪个实例/几何簇；
- 该像素在三角形内的重心坐标（用三个顶点的权重表示位置）。

它不存 albedo、法线、粗糙度这些属性结果。属性等到 resolve（解析：全屏逐像素拿 ID 回查几何、现场插值属性）阶段才算。

| | G-buffer（传统 deferred） | Vis-buffer |
|---|---|---|
| 每像素存什么 | albedo、法线、roughness、metallic、AO、运动向量…… | 三角形 ID（+ 实例/簇号）+ 重心坐标 |
| 大约字节数 | 常见 40+ B/px；本仓库精简到 20 B/px | 常见约 8 B/px（32 位 ID + 两个 16 位重心坐标），带实例/簇号会更多 |
| 属性从哪来 | 光栅化时算好，按附件格式量化写死 | resolve 阶段拿 ID 回查顶点数据现场插值 |
| 法线/UV 精度 | 受附件格式量化损失 | 从源数据重算，精度受浮点限制但不经附件量化 |

管线四步：

1. 光栅化全部不透明几何，每像素只写“被哪个三角形盖住 + 重心坐标”。深度测试照常，最终留下最前面的三角形。
2. 全屏 resolve：逐像素拿 ID 回查索引缓冲和顶点缓冲，用重心坐标现场插值法线、UV、切线；再拿材质 ID 走 bindless（着色器按槽号直接索引一张大描述符表，不用每次绘制重绑贴图）读材质参数。
3. 在这份“现在才凑齐”的材质上做光照。
4. 输出颜色。

它和 GDR 配套，有三条理由，环环相扣：

1. 微三角形无法在光栅化时预插值属性。Nanite 的目标是每帧处理数十亿级三角形，很多三角形的屏幕覆盖不到一个像素。如果每个三角形都在光栅化时插值整套属性并写 G-buffer，插值成本随三角形数爆炸，而每个像素最终只需要一个三角形的属性。vis-buffer 把属性计算推迟到“这个像素最终被谁盖住”确定之后，只为可见像素付一次。
2. bindless 是把同一个做法搬到像素层。本仓库 5.1 段把 CPU 每对象推 96B 参数换成 GPU 按 4B 槽号读表（push 是随单次绘制提交的小块参数；5.3 段起连 push 也退役，槽号走 `firstInstance`）；vis-buffer 让像素也只拿一个 ID 去表里取几何和材质，不把整套属性塞进像素。两者都是传索引、不传数据，前提都是表常驻显存、着色器能按槽号直接索引。本仓库的对象表（48B/行）、材质表（64B/行）和 bindless 贴图表已经在位，vis-buffer 需要的查表基础设施是现成的；缺的是几何侧按三角形 ID 提交和解析的路径。
3. 带宽。以传统 40 B/px 的 G-buffer 为例：4K（3840×2160 ≈ 830 万像素）下，写一遍约 332 MB，光照再读一遍又是 332 MB；60 fps 就是约 40 GB/s 的写读流量。vis-buffer 8 B/px 写读各一遍约 133 MB/帧，60 fps 约 8 GB/s，少一个数量级。这是忽略缓存与压缩的上限估算。id Tech 和 Frostbite 在 Nanite 之前就采用它（Frostbite 的叫法是 deferred material），主要动机就是这笔带宽开销。

## 四、G-buffer 与 vis-buffer 抢同一个职责位

管线上有个固定职责位：几何 pass 写下的每像素中间产物，留给后面的着色 pass 用。G-buffer 和 vis-buffer 抢的都是这个位置，三种管线形状：

```text
传统 deferred（本仓库当前形状）:
  光栅化：现场算属性 → 写 G-buffer → 延迟光照读 G-buffer → 颜色

UE5 Nanite 形状（deferred 渲染器下的链式融合）:
  光栅化：只写 ID → resolve 回表算属性 → 写进 G-buffer → 延迟光照照旧 → 颜色
                （vis-buffer 在 G-buffer 上游，resolve 就是 G-buffer 生成器）

纯 vis-buffer 形状（id Tech / Frostbite 的公开分享常被这样归类）:
  光栅化：只写 ID → resolve 回表算属性 → 直接算光照 → 颜色（没有 G-buffer）
```

UE5 的链式接法是融合选择：vis-buffer 只接管几何半边，resolve 完落回 G-buffer，后面整套光照/后处理一概不动，复用既有 deferred 管线。但这是特例不是定义：纯 vis-buffer 引擎 resolve 完直接着色，根本不落这份中间产物。所以不要把“G-buffer 由 vis-buffer 生成”当成 vis-buffer 的定义。

两种格式的存在前提正好相反，这是“什么时候需要 vis-buffer”的判据：

- G-buffer 的前提：光栅化时能算属性——每个三角形够大，插值成本能摊薄；带宽扛得住 20~40+ B/px 的写读。
- vis-buffer 的前提：光栅化时不该算属性——微三角形让插值成本爆炸，或带宽逼你瘦身。

判断顺序：没有海量微几何、没有带宽压力 → G-buffer 就是终态，换 vis-buffer 只是多一次全屏 resolve 和一堆随机读，通常反而更慢；出现了 → 纯替换（id Tech 式）或上游替换（UE5 式）。vis-buffer 永远不是“额外需要”的东西：它和 G-buffer 抢同一个职责位，上了它就要撤掉 G-buffer 的属性写入，或者把 G-buffer 的写者换成 resolve（UE5 式）。

举个具体例子。Sponza 这类静态建筑，大多数三角形在屏幕上覆盖多个像素，光栅化时插值属性很便宜，G-buffer 直接写就够；换 vis-buffer 只是多一次全屏 resolve 和随机读，没有收益。反过来，Nanite 式场景一帧数十亿个微三角形，大部分三角形覆盖不到一个像素，光栅化时给每个三角形插值整套属性再写 G-buffer，成本会爆炸；这时每像素只记 ID、只为最终可见的三角形 resolve 一次，才是对的。

## 五、vis-buffer 的三项代价：resolve、透明与 MSAA

- resolve 是一次全屏 pass，而且要按像素 ID 去索引/顶点缓冲做随机读。三角形越碎、相邻像素的三角形越不连续，缓存命中越差；属性插值成本从光栅化挪到全屏着色——只有可见像素付一次是赚，但 pass 结构变深、随机读的代价要另算。
- 透明冲突。混合（blend）要把新像素颜色和画面里已有颜色按透明度合成，必须逐像素读改写颜色；vis-buffer 阶段写的是 ID，同一张目标不能同时存 ID 和混合颜色，resolve 也无法与“画一层混一层”的顺序交织。所以半透明永远在这条流水线之外的独立 forward pass。G-buffer 也一样：它和 vis-buffer 都是先收集齐再统一着色，透明要画一层就混合一层，任何收集方式都挤不进去；区别只是 G-buffer 收集属性结果，vis-buffer 收集三角形 ID。
- MSAA 与运动向量要单独安排。vis-buffer 本身不带 MSAA 的多重采样语义，Nanite 走 TAA（时域抗锯齿，跨帧累积样本）这类方案；运动向量（记录像素这一帧相对上一帧移动了多少）也要另写一张目标，Nanite 另写 depth + motion。G-buffer 路线开 MSAA 要按样本存多份附件，通常也改用后处理抗锯齿；forward 路线则可以继续用硬件 MSAA。两条路线的抗锯齿选择，都是被“每像素存多少份”逼出来的。
- vis-buffer 省的是属性插值与 G-buffer 带宽，不省光栅化本身的覆盖测试。三角形多了，光栅化的固定成本还在；它不会因为像素格式变瘦而减少要处理的三角形数。

## 六、本项目落点

不透明是传统 deferred（G-buffer 写入 + 全屏光照合成），透明是 Clustered Forward（按光源分簇的前向层）；两条命令缓冲，透明单独一个 pass。没有 vis-buffer。

### 不透明：三张颜色附件 + 深度，20 B/px

`render/passes/gbuffer/gbuffer_targets.rs` 里的附件布局：

| 附件 | 格式 | 每像素 | 内容 |
|---|---|---|---|
| albedo | R8G8B8A8_UNORM | 4B | 线性反照率（贴图采样 × base_color，sRGB 在采样端解码） |
| normal | R16G16B16A16_SFLOAT | 8B | 世界空间法线 xyz |
| matparam | R8G8B8A8_UNORM | 4B | metallic / AO / perceptualRoughness / reflectance |
| depth | D32_SFLOAT | 4B | reverse-Z 深度（近处值大、远处值小，背景为 0） |
| 合计 | | 20B | 不含合成后写入的 HDR 目标；motion vectors 留槽不分配 |

`shaders/scene/gbuffer/gbuffer_write.wgsl` 在光栅化时直接算属性：采样 base color、metallic-roughness、occlusion、normal map，用屏幕空间导数现算 TBN（切线空间基），MASK 材质在片元裁切，最后写三张颜色附件。这就是“传统 deferred”的写者——没有三角形 ID，没有 resolve。BLEND 行由 `build_indirect` 把 `instanceCount` 置 0，交给透明层。

### 延迟光照：gbuffer_composite 读 G-buffer 与 cluster 列表

`shaders/scene/gbuffer/gbuffer_composite.wgsl` 是全屏光照累积 pass（管线名 `gbuffer_composite`）：读 G-buffer 的 albedo/normal/matparam/depth，查 cluster 光源列表和四级 CSM/PCSS 阴影（级联阴影贴图 + 百分比靠近软阴影），方向光乘阴影可见性，点光按 cluster 列表逐灯累加 Filament BRDF（描述表面如何反射光线的函数），再加环境光，写 HDR 中间目标（高动态范围颜色缓冲，R16G16B16A16_SFLOAT）。背景按 reverse-Z 深度 0 判定。

### 透明：Clustered Forward 的独立命令链

透明层在合成之后、同一 HDR 目标上混合：

- `shaders/scene/transparent/transparent_forward.wgsl` 用与不透明相同的方向光和 cluster 点光（不接阴影）、同一份 BRDF，alpha 混合写 HDR；深度从 G-buffer 重绑，只测不写。
- 光源分簇在 `render/passes/lighting/cluster.rs`：屏幕按 32×32 像素切格，深度方向再切 32 片（近密远疏的指数切分），近 0.1m 到远 200m，每格最多 16 盏灯；`cluster_build` 每帧算列表，不透明合成和透明前向都读它。
- 透明行由 `transparent_collect` 紧凑、GPU bitonic 排序（一种按固定比较网络在 GPU 上排序的算法，容量 16384，最远先画）、`transparent_gather` 写 20B 间接命令，透明前向一次 `cmd_draw_indexed_indirect` 画完。

没有 vis-buffer：`ash_renderer` 的代码与 shader 里没有三角形 ID 表，也没有全屏 resolve pass；G-buffer 的属性是光栅化时直接算的。所以按前面的职责位对照，本仓库是第一种形状。

### 与步骤 5 的关系：提交链没动，着色层整段换掉

- 步骤 5 的 compute 剔除、遮挡、LOD 与 indirect 闭环当时的定位是只换提交方式，不动着色层；当时的着色层还是单 pass forward 直出。
- 步骤 8 的主线是换着色层：不透明从直出改成 G-buffer + 光照累积，透明改成 Clustered Forward。它没有改步骤 5 的主视图提交链：G-buffer pass 仍然只发一次 `cmd_draw_indexed_indirect`，命令来自同一条命令缓冲，`firstInstance`（间接命令里携带实例槽号的字段）仍然按槽号取实例，没有逐对象 push；透明层另有一条命令缓冲，同样一次 indirect 画完。8.3 的级联阴影另外接了一条 GPU-driven 的 shadow 命令链，同样是这套提交机制的消费，不是把决策搬回 CPU。
- 8.1 的 G-buffer 多 pass 首跑就是在这条 indirect 消费链上换片元输出：顶点段继续按实例槽号读对象表/模型环/材质环，片元段从“直出颜色”改成“写三张附件”。两层的正交在这里最直观——提交方式没改，着色侧整段换掉。

### 升级路径：只剩 vis-buffer/deferred material 没走

外置拓展条目 5 登记的两级升级路径是：光源上量级走 forward+，微几何/带宽压力走 vis-buffer/deferred material。现在第一级的目标（多光源）已由步骤 8 的 cluster 光源列表实现——不透明改走 deferred 合成，透明走 Clustered Forward，两者共用列表；“从单 pass forward 升 forward+”这个具体路径没有发生，因为不透明先换成了 deferred。只剩第二级没走。

- 光源上量级：步骤 8 已经用 cluster 光源列表解决，不透明合成与透明前向共用一份列表；外置拓展条目 5 里“从单 pass forward 升 forward+”的前提已经不存在。
- 微三角形/带宽压力：vis-buffer / deferred material 是唯一没走的路。它在本仓库的形态会是上游替换：保留 `gbuffer_composite` 的光照累积和阴影，把 `gbuffer_write` 换成“只写三角形 ID + 重心坐标”的 pass，再加一个全屏 resolve 生成 albedo/normal/matparam（或直接进光照）。bindless 材质表和常驻几何池是现成的，缺的是几何侧的微三角形簇提交/光栅化路径——当前 indirect 仍是逐实例的索引绘制，不是按几何簇光栅化。
- 外置拓展条目 5 仍在 `.agents/docs/开发步骤/外置拓展需求/外置拓展需求.md` 的原编号处（条目 5，标题包含 forward+ 与 vis-buffer/deferred material）。注意它正文的“当前单 pass forward（3.5）”是登记时的状态，已被步骤 8 取代；读它时以本节现状为准。条目本身没有关闭，vis-buffer 那一半仍登记着。
- 信号判据：哪天对象表/材质表之外再出现“三角形 ID 表”，且有一个全屏 pass 来读它，就是踩上 vis-buffer 路线的信号。现在没有这张表。
- 学习价值：GDR 把“画哪个对象”从 CPU 决策换成 GPU 读表决策；vis-buffer 把“这个像素的材质是什么”从光栅化时算好换成像素只记三角形、着色时再回表查。两者是同一种做法的两个层级——决策权下放，像素格式是最后一环。

## 自测

1. 一个场景对象很多但只有一盏方向光，该不该上 GDR？该不该上 deferred？两个答案为什么可以不同？
2. 半透明为什么不能写进 G-buffer 或 vis-buffer？本仓库的透明层怎么处理深度和排序？
3. vis-buffer 每像素存什么？resolve 阶段拿这些数据做什么？
4. 什么条件下 vis-buffer 比 G-buffer 更合适？在本仓库现有几何规模下，上 vis-buffer 会多付什么？

## 相关阅读

- [GPU-Driven 本质](../可见性与GPU驱动/GPU-Driven%20本质.md)：决策与提交层的完整机制，本篇只借用它的结论。
- [为什么没有全上 GPU 驱动](../可见性与GPU驱动/为什么没有全上%20GPU%20驱动.md)：GDR 的适用边界；本篇讲它的着色层搭档。
- [8.1 功能流程导读](../../开发步骤/8-完整延迟渲染管线/8.1-G-buffer与不透明几何/8.1-功能流程导读：G-buffer与透明层的帧流走读.md)：本仓库 G-buffer 与透明层的逐时刻帧流。
- [外置拓展需求条目 5](../../开发步骤/外置拓展需求/外置拓展需求.md)：着色管线模式升级的登记原文（读时注意它停留在步骤 8 之前的现状）。

## 来源

- 初稿 2026-10-15，出处是 5.2 开工前三轮问答（“GPU-driven 一般用什么管线模式”“vis-buffer 是什么”“已有 G-buffer 还需不需要 vis-buffer”）；2026-11-19 按步骤 8 收官（8.1~8.6）后的工作区状态重写。
- 上游知识库：《GPU-Driven Rendering》《延迟渲染》（`C:\Users\zengkaixiang\.agents\docs\游戏制作\游戏引擎\渲染\渲染管线\`）。
- 行业调研口径（保留为调研结论，不在本仓库逐条复核）：
  - UE5 Nanite：visibility buffer + material resolve + deferred lighting 的链式融合；
  - Frostbite，GDC 2016 “Optimizing the Graphics Pipeline with Compute”：compute 剔除 + indirect；
  - Frostbite 的 deferred material 公开分享：几何 pass 只写可见性信息、resolve 阶段算材质；
  - Ubisoft，SIGGRAPH 2015 “GPU-Driven Rendering Pipelines”：compute 剔除 + indirect；
  - Unity 6 GPU Resident Drawer 官方文档（https://docs.unity.com/en-us/engine/6000.0/manual/analysis/graphics-performance-profiling/in-urp/reduce-draw-calls-urp/reduce-rendering-work-on-cpu/gpu-resident-drawer）：URP 仅 Forward+ 路径支持，需手动开启，只接管 Mesh Renderer；
  - id Tech（DOOM 系列）：clustered forward + mesh draw command + GPU 剔除；
  - COD 系：forward+ + GPU 剔除 + indirect。
- 本仓库代码：
  - `ash_renderer/src/render/passes/gbuffer/gbuffer_targets.rs`：G-buffer 附件格式、20 B/px、motion vectors 留槽；
  - `ash_renderer/shaders/scene/gbuffer/gbuffer_write.wgsl`：光栅化时直接算属性；
  - `ash_renderer/shaders/scene/gbuffer/gbuffer_composite.wgsl`、`ash_renderer/src/render/passes/lighting/composite_pipeline.rs`：延迟光照累积；
  - `ash_renderer/src/render/passes/lighting/cluster.rs`：32×32 像素格 × 32 深度片、0.1–200m、16 灯/格；
  - `ash_renderer/src/render/passes/transparent/transparent_passes.rs`、`ash_renderer/shaders/scene/transparent/transparent_forward.wgsl`、`ash_renderer/src/render/passes/transparent/transparent_pipeline.rs`：透明层的 collect/sort/gather 与前向混合；
  - `ash_renderer/src/render/frame/record.rs`：④ G-buffer、⑤ 合成 + 透明的帧编排；
  - `ash_renderer/src/render/resident/tables/instance_table.rs`：对象表 48B/行、材质表 64B/行、模型环 64B/行、间接命令 20B/条。
- 步骤 8 文档：`.agents/docs/开发步骤/8-完整延迟渲染管线/README.md`、`8.1-G-buffer与不透明几何/8.1-功能流程导读：G-buffer与透明层的帧流走读.md`、`8.1-G-buffer与不透明几何/8.1.5-施工记录：判定线总验收与计量.md`。
- 外置拓展：`.agents/docs/开发步骤/外置拓展需求/外置拓展需求.md` 条目 5（2026-10-15 登记，编号仍为 5；正文“当前单 pass forward”的前提已被步骤 8 取代）。
- 快照日期：2026-11-19（步骤 8 收官后的工作区状态）。
