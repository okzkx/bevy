# Unity SCGI 实现：源码级结构与数据流

要在自己的渲染器里复现 SCGI，概念层面的描述不够用，真正要回答的是这三个工程问题：射线得到的结果存在哪一种数据结构里；这些数据什么时候分配、什么时候回收、由谁回收；屏幕上的像素最后怎么从里面取到间接光。这篇按 Unity 7.0 alpha a9（`7000.0.0a9`）内置包里的实际源码回答这三个问题，常量和公式都来自代码，不是从博客转述。

全篇的核心不变式只有一条：

> **屏幕像素从不发射线。射线只为世界空间的 patch 服务，像素最后只做查表和滤波。**

读懂这句话，后面的结构都是它的推论：为什么缓存要按世界空间分格、为什么 patch 要有分配与回收、为什么屏幕侧只需要低分辨率查找加一次上采样。

## 一帧里的执行顺序

SCGI 的帧内工作分成两半：世界更新一帧只做一次，供所有相机共用；每个相机再各自执行一遍缓存更新和屏幕解析。世界更新负责把场景里变了的网格、材质、灯光、自发光三角形同步进加速结构；相机侧负责在缓存里分配 patch、发射线、滤波、最后写出一张屏幕空间的辐照度纹理。

| 阶段 | 做什么 | 主要代码 |
|---|---|---|
| 世界更新 | 收集场景变更，更新 BVH、材质图集、灯光、自发光三角形列表 | `SurfaceCacheWorldPass.RecordRenderGraph`、`SurfaceCacheWorld.Commit` |
| 平面法线 | 从深度重建每个像素的平面法线 | `FlatNormalResolution.compute` |
| 缓存准备 | Scrolling → Eviction → Defrag | `SurfaceCache.RecordPreparation` |
| patch 分配 | 在 1/4 分辨率屏幕上生成 patch | URP 的 `PatchAllocation.compute` |
| 缓存更新 | Estimation → 空间滤波 → 时域滤波 → 可选 bounce 分配 | `SurfaceCache.RecordUpdate` |
| 屏幕查找 | 在 1/4 分辨率上查询世界缓存 | `ScreenResolveLookup.compute` |
| 屏幕去噪 | 0～4 遍低分辨率去噪 | `ScreenResolveDenoising.compute` |
| 上采样 | 双边上采样，并用着色法线求值 SH | `ScreenResolveUpsampling.compute` |
| 消费 | 写入 `irradianceTexture`，供不透明物体着色 | `SurfaceCacheCameraPass` |

有两个顺序点值得记住。第一，世界更新在所有相机之前，且一帧只执行一次；相机缓存是每个相机独立的，切换相机或 XR 双眼会各自持有一份。第二，整个 SCGI 链路在 prepass 之后、不透明物体之前完成，因为它在着色阶段要提供 `irradianceTexture`；启用 SCGI 时还有一个 `SurfaceCacheZeroGIPass` 先把这张纹理设成黑色，保证不透明物体的 shader 走屏幕空间 GI 分支，而不是退回 lightmap 或 APV。

## 数据住在哪：体积格子与 patch 池

SCGI 的缓存不是一个放在屏幕上的缓冲区，而是两层结构叠在一起：一层是随相机移动的三维格子，负责"按位置和方向找到 patch"；另一层是定长的 patch 池，负责"存每个 patch 的辐照度"。格子本身不存光，只存索引。

这里的 patch 不是 GIBS 的 surfel。surfel 的身份是表面元素，带半径、随表面移动和合并分裂；Unity 的 patch 身份是"cell + 方向桶"，位置和法线只是每帧从屏幕采样里选出的代表表面点。缓存的组织是空间网格，缓存的内容才是表面点的辐照度。

### 体积格子

体积是一个相机跟随的立方体，由三个参数决定：`Size`（世界单位边长）、`Resolution`（每边的空间格子数）、`CascadeCount`（级联数）。每级级联的格子大小翻倍，覆盖范围也翻倍，因此近处细、远处粗，不需要把整个视野都铺满精细格子。

每个空间格子再按方向切成 16 份（`AngularResolution = 4`，4×4），方向不是按球面均匀分，而是把方向向量做八面体映射到正方形上取格。代码里还先乘了一个固定的旋转矩阵，避免地平面、墙面这类轴对齐表面的法线全部落在方向格的边界上。一个 cell 的唯一索引是：

```
cellIndex = cascadeIdx × (res³ × 16)
          + (x × res² + y × res + z) × 16
          + directionIndex
```

空间格子的最小尺寸由体积大小和级联数共同决定：

```
VoxelMinSize = Size / (Resolution × 2^(CascadeCount−1))
```

按默认的 `Size = 128`、`Resolution = 32`、`CascadeCount = 4` 计算，最细一级的格子是 0.5 米，正好落在官方文档建议的 0.25～0.5 米范围内。中等档位的 cell 总数是 `32³ × 16 × 4 = 2,097,152`；Ultra 档 `Resolution = 64` 时是 `16,777,216`，每个 cell 一张 uint 表，单张就是 64MB，`CellPatchIndices` 和 `CellAllocationMarks` 两张表加起来 128MB。这是 SCGI 显存开销里最大的一块，比 patch 池大得多。

三张格子相关的表分工如下：

- `CellPatchIndices`：cell → patch 索引，查询时只读这一张；
- `PatchCellIndices`：patch → cell 索引，回收和整理时用；
- `CellAllocationMarks`：分配占位标记，防止同一帧里多个线程重复给同一个 cell 分配 patch。

级联偏移量不是连续值，而是按整格对齐的：相机移动超过半格，`CascadeOffsets` 才会更新一次。这样数据不需要跟着相机平移，只需要改变"存储空间坐标"和"体积空间坐标"之间的换算基准。查询时用 `SignedIntegerModulo` 把坐标转成存储坐标，正好落在格子边界上的点也不会因为浮点误差在两个相邻格之间来回跳。

源码对轴对齐平面的闪烁还做了一层修补：在量化到格子之前，先加一个世界坐标驱动的准周期波形偏移，幅度是 0.01 个格子（`PatchUtil.GetVoxelWarpOffset`）。它不是彻底的修复，但能把"地面正好在 y=0、法线正好朝上"这类退化情况从固定格边界上偏移开。预览帖里提到的"与 x/y/z 轴完全平齐的平面会闪"，对应的就是这段代码。

### patch 池

patch 池是定长的 65536 条，选这个数有一个硬性原因：HLSL 侧的环形分配用 `UINT_MAX % patchCapacity` 做回绕，`2³²` 必须能被容量整除，65536 正好满足，C# 侧还有一条 `Debug.Assert` 保证这个前提。

每条 patch 存四类数据：

- `PatchGeometries`：位置和法线，两个 float3；
- 三份 `RGBL1` 辐照度，每份 12 个 float：`RGBL1` 是红绿蓝三通道各 4 个球谐系数（L0 一个加 L1 三个），合起来 12 个 float；
- `PatchStatistics`：`mean`、`variance`、`irradianceState`、`requestState`；
- `PatchCellIndices`：反向索引，指回它占用的 cell。

`requestState` 和 `irradianceState` 都是位打包的：

- `requestState` 低 8 位是 rank，高 16 位是 heartbeat 帧号；heartbeat 用 16 位取模，靠 `ModuloDistance` 计算"距离上次心跳多少帧"；
- `irradianceState` 低 16 位是更新次数，第 16 位是"时域缓冲是否已初始化"。

三份辐照度不是冗余，而是流水线的三个位置：Estimation 写第 0 份；空间滤波从第 0 份读到第 1 份；时域后滤波把第 1 份（空间滤波关闭时是第 0 份）混合进第 2 份；patch 分配时则同时把种子写进第 0 份和第 2 份，保证新 patch 的两条路径都从同一个初值开始。

按每条 patch 约 204 字节估算，65536 条大约 13MB；主要开销是体积格子和 BVH、材质图集。这也是为什么 `Resolution` 从 32 调到 64 会明显占用显存，而 `Size` 变化不会重新分配格子，只会改变格子尺寸。

### 另外三块世界数据

patch 池之外，`SurfaceCacheWorld` 还维护三样东西：

- 加速结构：`AccelStructAdapter` 包着 Unity 的 BLAS/TLAS（前者是每个网格的加速结构，后者是实例级顶层结构），每个实例记录网格、变换、每 submesh 的材质索引与是否不透明；
- 材质池：`MaterialPool` 把 Unity 材质通过 Meta pass 转换成描述符，再把 albedo、emission、transmission 贴图复制进三张图集，shader 里用纹理索引加 UV 变换采样；
- 灯光与自发光：一个方向光、一个点光/聚光灯数组、一个自发光三角形列表（每个三角形两个 uint），以及一张 128 采样的点光预采样表。

环境光来自一张 128 分辨率的 cubemap；只有在 cubemap 哈希或环境强度变化时，才重算 16 个方向的全局探针（`GlobalProbe.compute`），供体积外兜底使用。

## patch 怎么分配：两种生成方式

patch 的分配来源有两个，分别对应"相机看得见"和"射线打得着"。

### 屏幕生成

每个相机每帧在 1/4 分辨率的低分辨率屏幕上执行一遍 `PatchAllocation`。每个低分辨率像素映射回全分辨率位置，读深度和平面法线，反投影出世界坐标，再算出它落在哪个级联、哪个 cell、哪个方向格。

这个 pass 有几个不太显眼但很关键的细节：

- 低分辨率像素的遍历顺序每帧会被 `RandomPermute` 打乱，避免相机不动时总是同一批像素先占到 patch；
- 全分辨率采样点不是固定取低分辨率像素的左上角，而是在 4×4 的 16 个偏移里逐帧轮换（`_FullResPixelOffset`），这样一帧只采 1/16 的像素，16 帧覆盖全分辨率；
- 如果 cell 已经有 patch，会刷新它的位置、法线、rank 和 heartbeat，把它标成"本帧相机可见"，rank 设为 0；位置和法线必须刷新，因为下一帧 Estimation 会拿它们当射线起点和半球朝向；辐照度和统计量保持不动，历史继续累积；
- 如果 cell 是新分配的，会尝试用运动矢量把上一帧的低分辨率辐照度重投影过来做种子；只有当重投影点与当前 patch 平面的距离小于一个格子时，才接受这个种子，并把更新次数设成 `updateMax / 2 = 16`。否则从零开始。

这套生成方式决定了 SCGI 的响应速度：相机一转头，新露出来的表面在当帧就拿到 patch，但辐照度要么从上一帧重投影、要么从零开始，需要几帧才收敛。

### bounce 生成

第二条分配路径在 Estimation 内部。每根半球采样射线打到表面时，会去查命中点的多弹射缓存；如果命中点不在缓存里，就通过原子计数往 `_PatchAllocationRequests` 里追加一条请求，最多 1024 条。随后单独执行一个 `PatchAllocation` pass（核心包里的版本），在这些命中点位置分配新 patch。

bounce 分配的 patch 初始 rank 是 1，和屏幕生成的 rank 0 区分开。rank 的语义是"这个 patch 的光是不是相机直接可见的表面贡献的"：屏幕生成的 patch 是相机看得见的，rank 0；射线打到才产生的 patch，是间接可见的，rank 1。屏幕查询时只接受 rank 0 的 patch，bounce 查询时才允许读 rank 1。这样从根本上避免了"相机看不见的角落把光漏进相机看得见的表面"。

如果 patch 池满了，分配会失败并回退：`count` 被写回容量值，`CellAllocationMarks` 清零，这个 cell 这一帧就保持没有 patch。

## patch 怎么回收：滚动、驱逐、整理

缓存是定长的，所以必须有回收。SCGI 用了三套互补的机制，分别处理"相机移动出级联范围"、"patch 太久没用"和"池子里有空闲槽位"。

### Scrolling：相机移动出级联范围时重映射

每个级联的偏移量按整格对齐。相机移动导致某个级联的偏移量变化时，`Scrolling.compute` 会遍历这个级联的每个存储格，把它换算回体积空间坐标；如果换算后落在体积范围之外，就说明这个位置以后不会再被查询，于是把该格 16 个方向上的 patch 全部释放。

注意它不移动数据。存储位置不动，变的是存储坐标与体积坐标之间的偏移量。这样滚动本身几乎不产生开销，代价是滚出范围的 cell 要重新生成。

### Eviction：心跳超时

每个 patch 有一个 16 位的 heartbeat，记录最近一次被刷新时的帧号。`Eviction.compute` 用 `ModuloDistance` 算"距离上次心跳多少帧"，超过 `60 × 4 = 240` 帧就回收。

如果开了 bounce patch allocation，回收分两步：rank 0 的 patch 先降级成 rank 1，刷新心跳，等下一轮再真正回收；rank 1 的 patch 直接回收。这样"相机刚刚看不见、但射线可能还在用"的 patch 能多活一轮，避免刚被屏幕生成过的表面立刻又被 bounce 请求重新分配。

### Defrag：让有效 patch 连续排列

环形池里的 patch 会因为驱逐出现空闲槽位。`Defrag.compute` 每帧执行 `DefragCount` 次迭代（档位里是 2/4/8/16），每次在 wave（GPU 上一起执行的一组线程）内用前缀和统计可回收的 patch，把后面的有效 patch 向前移动，并同步更新 `CellPatchIndices`、几何、三份辐照度和统计量。

整理时读和写用两份 ring config 交替（flip-flop），每轮迭代翻转一次；`_PatchOffset` 在奇偶迭代里错开半个 subgroup（GPU 上一起执行的一组线程），避免读写同一批元素产生竞争。整理完成后，`count/start/end` 三个值一起更新，环形缓冲的语义保持不变。

这三套机制合起来的效果是：新 patch 总能从环的尾部拿到空位；老 patch 在 240 帧没有被任何相机或射线刷新后被回收；整理把碎片压缩，让 65536 条的池子保持连续可用。

## 射线怎么发：Estimation 的采样结构

`Estimation.compute` 对 65536 个 patch 各起一个线程，但只有落在 ring 有效区间里的 patch 才真正工作，所以实际成本跟"当前有效的 patch 数"走，不是跟容量走。

每个 patch 每帧拿 `SampleCount` 个样本（低/中/高/超高分别是 1/2/4/8）。如果这个 patch 从来没被更新过，样本数再乘 `WarmUpSampleMultiplier`（1/2/4/8），用来让新露出的表面更快收敛。样本偏移量按帧递增，随机数用 `QrngKronecker2D`（低差异序列）从 patch 索引哈希和样本偏移初始化，保证同一 patch 不同帧的样本不重复。

每个样本不是一个完整的路径，而是三个独立的采样域，各自由自己的随机数序列驱动：

### 半球采样：环境、方向光、多弹射

在 patch 法线周围的半球上均匀采一个方向，打一条最近命中射线：

- 没打到任何东西：取环境 cubemap 的颜色；
- 打到背面（且材质不是双面 GI）：判定 patch 在几何体内部，这个样本作废，不累加。这样 patch 暂时被移动物体包住时，缓存会暂停更新，而不是把"黑暗"累加进去；
- 打到正面：读命中点的材质和法线，算方向光的可见性，再查命中点的多弹射缓存；如果命中点有自发光，按面积采样和半球采样的概率做 MIS 加权。

多弹射查的是命中点自己的 `RGBL1` 辐照度，用命中点法线求值。这就是"缓存自反馈"：patch A 的射线打到 patch B，B 的辐照度参与 A 的估计；下一帧反过来也成立。

### 点光采样：128 条预采样加重连接射线

点光和聚光灯不按 patch 逐个采样，而是先执行一个独立 pass：`PunctualLightSampling.compute` 发射 128 条射线，每条随机选一个灯，从灯的位置在锥角内采一个方向，打到表面就记录命中点、法线、反照率、距离和概率密度。

Estimation 阶段每个 patch 只需要从这 128 条里随机挑一条，从 patch 位置向命中点打一条"重连接射线"，验证中间没有遮挡、命中的还是同一个实例和同一个三角形。通过验证后，用一套已经化简的 Jacobian 公式算贡献：把 patch 的立体角测度经命中点面积测度换到光源立体角测度。这样每帧只花 128 条灯射线，就能让所有 patch 共享同一批灯样本，成本与 patch 数解耦。

### 自发光三角形采样：面积采样加 MIS

自发光不靠环境光，也不靠点光：单独维护一个自发光三角形列表。每个 patch 样本随机选一个三角形，按面积均匀采一个点，打一条阴影射线；同时用半球采样打到同一个三角形的概率做 MIS，避免自发光被重复计算。自发光颜色从材质图集读取，强度乘一个全局系数。

三个采样域的结果都投影到 L1 球谐（`ProjectAndAccumulate`），然后做一次余弦卷积（`CosineConvolve`），从"入射辐亮度"转成"辐照度"，最后写进 patch 的辐照度缓冲。球谐只保留 L1，一条 patch 12 个 float，这是 SCGI 能把缓存压到十几 MB 的前提。

射线起点有一个额外偏移：`OffsetRayOrigin` 之外再加 `VoxelMinSize × 0.001`。原因是 patch 的位置来自光栅化深度的反投影，精度不如重心坐标重建，不加这个偏移会在薄几何上出现自交。

## 收敛怎么做的：从 patch 到屏幕的四级滤波

单帧样本数很少（默认 2 个），噪声靠时域和空间滤波抑制。SCGI 一共四层，每层的输入输出和权重都不一样。

### patch 级时域：方差引导的滞后

Estimation 写完一个 patch 后，不是直接覆盖旧值，而是做一次带方差引导的混合（`FilterTemporallyVarianceGuided`）：

- 先算短期均值与方差，方差来自新样本与短期均值的偏差；
- 漂移量 `drift = |短期均值 − 旧 L0| / 标准差`；漂移大说明光照真的变了，权重应该低一些、让新值更快进来；
- 更新权重在长期滞后 0.995 和短期滞后之间插值；短期滞后由 `TemporalSmoothing` 映射到 0.75～0.95；
- 如果 patch 的更新次数还没到 `updateMax = 32`，不用指数混合，直接用精确的累计平均 `old / (old + new)`，因为样本少的时候指数平均会引入偏差。

`updateMax = 32` 之后，指数混合接管；这也是为什么时间平滑度只影响稳定后的行为，不影响新 patch 的收敛速度。

### patch 级空间滤波

`SpatialFiltering.compute` 在 patch 的切平面上按半径 `SpatialFilterRadius × 格子尺寸` 投若干抖动采样点，每个点用 `ReadHemisphericalIrradiance` 查一次缓存，权重是 `1 − |jitter| / √2`，最后归一化。它只在 patch 的切平面内取样，权重随距离衰减；边缘处查不到 patch 的采样会被跳过，不会强行混合。

### patch 级时域后滤波

`TemporalFiltering.compute` 在空间滤波之后再做一次简单的指数混合，权重 `lerp(0.9, 0.95, TemporalSmoothing)`；第一帧不做混合，直接取新值。它的作用是抑制空间滤波之后残留的高频闪烁。

### 屏幕级：低分辨率查找、去噪、上采样

前面所有滤波都发生在世界空间。到屏幕侧，SCGI 只在 1/4 分辨率上查缓存：

- `ScreenResolveLookup` 对每个低分辨率像素取一个中心样本（权重 0.01，不做 rank 检查），再取 `LookupSampleCount` 个抖动样本；位置在格子尺寸内抖动，方向在 `cos = 0.9` 的锥内抖动；抖动样本只接受 rank 0 的 patch；
- 查不到且开了 `DistanceFallback` 时，用全局探针按方向取值；
- 结果压缩成 L1：`l1 × √3 / (4 × l0) + 0.5`，保证落在 [0,1]，L1 存 RGBA8；L0 存 RGBA16F。压缩的前提是辐照度已经做过余弦卷积，否则 L1 可能越界；
- `ScreenResolveDenoising` 做 0～4 遍 3×3 滤波，每遍的步长按 `{1, 2, 2, 3}` 取，权重是高斯 × 法线相似度（8 次方）× 平面距离，既抑制噪声又尽量不跨边缘；
- `ScreenResolveUpsampling` 用 4 个低分辨率邻居做双边上采样：双线性权重 × 法线点积 × 平面距离指数衰减，再加 0.01 倍双线性权重兜底；最后用全分辨率的着色法线对 SH 求值，乘上强度，写进 `irradianceTexture`。

因此屏幕侧的成本与分辨率成正比，与 patch 数和射线数无关；射线成本才与 patch 数和样本数成正比。官方调优文档把这两类成本分开讲，原因就在这里。

## 参数面与质量档

`SurfaceCacheGIVolumeOverride` 把可调项分成五组：开关与强度、光传输、patch 滤波、屏幕滤波、体积。质量档只是把这五组里的十二个参数打包，切到 Custom 后可以逐项改。

| 参数 | Low | Medium（默认） | High | Ultra |
|---|---|---|---|---|
| `volumeResolution` | 16 | 32 | 48 | 64 |
| `lightTransportMultiBounce` | 关 | 开 | 开 | 开 |
| `lightTransportSampleCount` | 1 | 2 | 4 | 8 |
| `lightTransportWarmupSampleMultiplier` | 1 | 2 | 4 | 8 |
| `volumeDistanceFallback` | 关 | 关 | 开 | 开 |
| `patchFilteringTemporalSmoothing` | 0.9 | 0.8 | 0.7 | 0.6 |
| `patchFilteringSpatialEnabled` | 关 | 开 | 开 | 开 |
| `patchFilteringSpatialSampleCount` | 4 | 4 | 6 | 8 |
| `screenFilteringLookupSampleCount` | 2 | 4 | 8 | 16 |
| `screenFilteringDenoisingPassCount` | 1 | 2 | 3 | 3 |
| `lightTransportDefragCount` | 2 | 4 | 8 | 16 |

另外几个不随档位变的默认值：`volumeSize = 128`、`volumeCascadeCount = 4`、`volumePatchWarpingEnabled = 开`、`patchFilteringPostTemporalEnabled = 开`、`lightTransportBouncePatchAllocation = 关`、`intensity` 范围 0～4。

调参时的成本模型很简单：

- 射线成本 ∝ 有效的 patch 数 × 每 patch 样本数 × 场景求交成本；
- 屏幕成本 ∝ 分辨率 ×（查找样本数 + 去噪遍数）；
- 改 `Resolution` 或 `CascadeCount` 会重建整个缓存，帧号归零、需要重新预热；改 `Size` 只改格子尺寸，不会重建；
- `DistanceFallback` 只影响体积外的像素，不增加体积内的射线成本。

## 场景怎么进来：宿主侧的变更同步

SCGI 不要求场景是静态的，但它需要知道"哪些东西变了"。Unity 用 `ObjectDispatcher` 收集每帧的变化：变换、材质、网格销毁、地形、灯光。`SurfaceCacheWorldAdapter` 拿这份变更集去更新自己维护的实例集合，再把结果提交给 `SurfaceCacheWorld`，由它重建 BLAS/TLAS 的相关部分。

几个实现上的取舍：

- 只跟踪 `MeshRenderer` 和实体实例，不跟踪 `SkinnedMeshRenderer`；蒙皮网格可以接收间接光，但不会把光反弹到周围，这和官方文档的说法一致；
- 材质通过 Unity 的 Meta pass 转换，因为 Meta pass 本来就是给 GI 系统提供 albedo/emission 的约定；转换后的贴图被复制进三张共享图集，材质只存图集索引和 UV 变换；
- 自发光三角形单独维护，用加法和删除两个 compute kernel 做双缓冲，只有材质从非自发光变成自发光（或反过来）时才重新跟踪实例；
- 加速结构按变更增量更新：Compute 后端在实例变化时重建 TLAS，硬件后端每帧提交一次构建命令、由 GfxDevice 内部处理，BLAS 按网格缓存；环境 cubemap 只在天空盒或环境光变化时重画；
- 渲染层遮罩可以在世界更新这一层排除对象，让它们既不进 BVH 也不贡献间接光；这比在每个 pass 里过滤开销更低。

相机侧的状态按 `(相机, XR multipass id)` 缓存，连续两帧没用就销毁；`Resolution` 或 `CascadeCount` 变化时销毁重建，`FrameIndex` 归零。`enabled = false` 时不做任何逐帧工作，缓存也丢弃，所以重新开启需要一次预热——文档里说的"切换有代价"就是这个。

## 屏幕端怎么消费

上采样的结果被赋给 URP 的 `resourceData.irradianceTexture`，shader 侧通过 `_SCREEN_SPACE_IRRADIANCE` 关键字和 `_ScreenSpaceIrradiance` 纹理读取：

- `SampleScreenSpaceGI` 用 `positionSS` 直接 `LOAD_TEXTURE2D_X`，不做采样器过滤，因为纹理是 1/4 分辨率上采样出来的，过滤已经在 upsample pass 里做了；
- 纹理存的是预曝光值，读取时乘 `invPreExposureMultiplier` 还原，否则 fp16 存不下高动态范围；
- 透明表面不在缓存里，`SAMPLE_GI` 直接退回环境探针；
- 启用 SCGI 时 `InitializeBakedGI` 完全绕过 lightmap 和 APV，不会出现"SCGI 加烘焙"的叠加。

## 和 Kajiya 的 irradiance cache 对照

Kajiya 是 Unity 之外最接近的公开实现：同样是相机跟随的级联格子、同样存 L1 球谐、同样用 rank 防止漏光。但两者服务的渲染器形态不同，同一套思想产生了不同的工程取舍。把两边的实现并排对照，能看清哪些设计是世界空间缓存的共性，哪些是 Unity 为光栅化管线做的取舍。

| 维度 | Kajiya | Unity SCGI |
|---|---|---|
| 系统形态 | 路径追踪器，缓存给非首弹射线提供后续弹射的近似 | 光栅化管线，缓存给不透明像素提供间接漫反射 |
| 空间组织 | 12 张相机对齐的稀疏 clipmap（稀疏体素层级），每级 32³；顶层 32³ 密集索引指向 65536 条目的 payload（条目数据）池 | 1～8 级相机跟随体素，每级 16～128 分辨率；每个体素再分 16 个方向桶；patch 池定长 65536 条 |
| 条目身份 | 体素 → payload 索引，没有方向桶 | cell + 方向桶 → patch 索引 |
| 条目内容 | 4×4 八面体图分层的 ReSTIR 蓄水池（每个条目维护一组候选样本），最终卷积成 L1 SH | 位置、法线、L1 SH、mean/variance/updateCount/rank/heartbeat |
| 分配驱动 | 每帧每体素从查询点里选一个投票点，按实际使用量投放（output-sensitive） | 1/4 分辨率屏幕生成 + 射线命中点的 bounce 请求（最多 1024 条/帧） |
| 查询路径 | 屏幕像素不直接查缓存；射线命中点查，用于后续弹射 | 屏幕像素在 1/4 分辨率直接查缓存，再去噪、上采样 |
| 每帧射线预算 | 每体素一条投票射线 + 每条目 4 条验证射线 | 每 patch 1/2/4/8 个样本，乘预热倍率 |
| 时域/空间滤波 | ReSTIR 蓄水池 + 时域累计；无空间平滑，时域不稳是已知缺点 | 方差引导的时域混合 + patch 空间滤波 + 时域后滤波 + 屏幕去噪/上采样 |
| 漏光控制 | rank = 最低弹射级，屏幕可见为 1，逐级递增；新投票只有 rank ≤ 旧值才生效；法线偏置 | rank 0 = 相机可见，rank 1 = bounce 生成；屏幕查询只读 rank 0；另有 patch warping 处理轴对齐退化 |
| 回收 | 条目几帧不被查询就回收 | 240 帧 heartbeat 超时回收；滚动释放出界 cell；defrag 压缩空闲槽位 |
| 成本与平台 | ~0.55ms（RX 6800 XT，1080p）；依赖光追硬件 | 质量档可调；硬件 RT 或 compute 回退；面向 URP 与移动端 |

最大的差异是**谁驱动分配**。Kajiya 的缓存跟随射线的实际使用：每帧每个体素从查询点里选一个投票点作为射线起点，缓存条目出现在射线真正打到的地方；屏幕像素不直接查缓存，而是先打一条弹射射线，再由命中点查。Unity 的缓存跟随屏幕可见性：低分辨率屏幕每帧把可见表面的 cell 刷新成 patch（已有则保活，没有才分配），屏幕像素可以直接在自己的位置查缓存，不需要先打一条弹射射线；射线命中点只通过 bounce 请求补充间接可见的 patch。前者更适合路径追踪器的形态，后者更适合光栅化管线；代价是 Unity 每帧都要遍历一遍低分辨率屏幕，并且依赖运动矢量做新 patch 的重投影种子。

**方向桶是 Unity 特有的。** Kajiya 的条目按体素组织，用查询点沿法线偏移来压制薄几何的双面漏光；Unity 把每个体素再按法线分成 16 个方向桶，薄墙两侧可以各有一条 patch，不会互相覆盖。这会把 cell 数量放大 16 倍（也是 SCGI 显存的最大一项），但对游戏场景里大量的薄墙、地板、双面几何更稳健。

**条目的采样结构不同。** Kajiya 在每个条目里放 4×4 八面体图分层的 ReSTIR 蓄水池，每帧 4 个格点出新候选 + 4 条验证射线，单条目的估计质量更高，但文档自己把“时域不稳、无空间平滑采样”列为缺点。Unity 的条目没有 ReSTIR 蓄水池，只存统计量（mean/variance/updateCount）、rank/heartbeat、几何和 L1 SH，用方差引导的滞后做时域混合，把去噪的主要工作放到 patch 空间滤波和屏幕侧的去噪、上采样。结果是 Unity 更稳定但空间细节更少；Kajiya 的细节更锐利，但更容易出现时域闪烁。

**rank 的编号不同，语义相同。** 两者都用“最低弹射级/可见级别”阻止只通过间接路径可见的条目把光漏进屏幕可见的表面。Kajiya 把屏幕可见的条目记为 rank 1，逐级弹射递增；Unity 把相机可见的 patch 记为 rank 0，bounce 生成的记为 rank 1，屏幕查询只接受 rank 0。Unity 还把这个 rank 用在回收上：开了 bounce allocation 时，rank 0 的 patch 先降级成 rank 1，再等下一轮心跳超时才真正回收。读两边的 rank 时不要看具体数字，要看那条不变式：**屏幕可见的查询只允许读屏幕可见来源的条目。**

两边的收益与代价可以并排列出：

| 实现 | 收益 | 代价 |
|---|---|---|
| Unity SCGI | 可见表面天然有 patch，屏幕像素直接查缓存，不需要首弹射线；O(1) 寻址、固定内存、适合 GPU 并行；方向桶让薄墙和双面几何各有一条 patch；方差引导 + 空间滤波 + 屏幕去噪带来更好的时域稳定性；质量档和固定预算适合 URP 与移动端 | 每帧遍历全部低分辨率像素，屏幕侧成本随分辨率走；新 patch 依赖运动矢量做重投影种子；格子尺度以下的细节丢失，同一格里多个表面竞争；16 个方向桶让 cell 内存放大 16 倍（64 档两张 cell 表 128MB）；只做漫反射，a9 的 cutout 不覆盖、透明不参与；需要关静态合批、Raw mesh buffer，每个相机一份缓存 |
| Kajiya | 缓存跟随射线使用，按实际需求分配；每条目用 ReSTIR 蓄水池，估计质量更高、细节更锐；没有方向桶，cell 内存更省；缓存分配不依赖屏幕可见性；rank 与投票对漏光更稳健 | 路径追踪器专用，屏幕像素要先打一条弹射射线才能查到缓存；时域不稳、无空间平滑，噪声和闪烁更明显；每条目 ReSTIR（4×4 八面体图 + 验证射线）实现复杂；投票本身是一套额外机制；依赖硬件光追，成本面向高端 GPU |

这不是谁更好的问题，而是交换的维度不同。Unity 用内存和每帧屏幕遍历，换直接屏幕查询、时域稳定和光栅化集成；Kajiya 用首弹射线、投票复杂度和时域噪声，换 output-sensitive 的射线预算和更高的单条目质量。

对 ash_renderer 的选型含义是：如果最终形态是“光栅化 + 世界空间缓存”，Unity 的屏幕生成模型接入更直接；如果最终形态是路径追踪器，Kajiya 的投票模型更自然。第一版建议先用 Unity 的结构（cell + 定长 patch 池 + 屏幕生成），每条目只做方差引导的时域混合，不引入 Kajiya 的 ReSTIR 蓄水池；等薄几何漏光真的成为问题，再考虑加方向桶或法线偏置。

## 哪些能移植到 ash_renderer

把 Unity 的实现按模块看，可移植的部分和不可移植的部分界限很清楚。

**可以直接借鉴的机制**：

- 体积格子 + 定长 patch 池 + cell/patch 双向索引的结构；
- rank 与 heartbeat 的组合：rank 控制漏光，heartbeat 控制回收；
- 环形池的分配、驱逐、整理三组机制，以及 `2³²` 整除容量的约束；
- 方差引导的时域混合、空间滤波、低分辨率查找、双边上采样这条四级滤波链；
- 质量档与成本模型：射线成本跟 patch 数走，屏幕成本跟分辨率走。

**需要重写的部分**：

- Unity 宿主层（RenderGraph、GraphicsBuffer、ContextContainer）换成自己的命令缓冲和描述符管理；
- `UnifiedRayTracing` 的加速结构换成 Vulkan 的 `VK_KHR_acceleration_structure` + `VK_KHR_ray_query`，或者自己搭 compute 软求交；
- 材质转换（Meta pass）、ObjectDispatcher 变更检测、实体桥接这些 Unity 专有接口，换成 Bevy 侧的资产事件与变更检测；
- 自发光三角形列表、材质图集这些数据组织可以照结构重建，但代码不能直接复用。

对 ash_renderer 来说，真正的新基建是"射线怎么求交"。两条路都可行：

- 硬件路径：RTX 2060 支持 `VK_KHR_ray_query`（本机 `vulkaninfo` 已核实），用 BLAS/TLAS 加 inline ray query，和 Unity 的 Hardware 后端是同一形态；
- 软件路径：Unity 的 Compute 后端自带一套 `HlbvhBuilder` + `RestructureBvh` + RadeonRays 的 BVH，没有硬件光追时也能工作；这条路的概念可以移植，但实现量比硬件路径大得多。

一个务实的最小版本可以按这个顺序做：单级联体积 + 屏幕生成 patch + L1 SH + 低分辨率查找与上采样，先不做 bounce、驱逐和整理；验证通过后再加 heartbeat/rank、bounce 分配和 defrag。这样第一版就能看到"间接光随几何和灯光变化"，而不是一次性把所有机制全部实现。

显存要提前算：6GB 的 2060 上，`Resolution = 48` 的 CellPatchIndices 加 CellAllocationMarks 已经是 56MB，`Resolution = 64` 是 128MB，再叠加 BVH、材质图集和屏幕缓冲。建议从 32/4 级联起步，确认预算后再往上调。

## 源码给出的边界

以下限制在 a9 的源码里可以直接验证，和官方文档能对上：

- **只做漫反射**：材质在采样时被固定成 `roughness = 1`、`metalness = 0`，BRDF 只有 `albedo / π`，没有镜面间接光。
- **alpha cutout 未支持**：SCGI 的路径追踪只读 albedo 的 rgb，不读 alpha，也没有 `discard`；6.7 文档同样写明 cutout 按不透明三角形处理，7.0 a16 才加入。
- **透明表面不参与**：屏幕着色时退回环境探针，缓存里没有透明物体的 patch。
- **蒙皮网格只接收不反弹**：适配器只跟踪 `MeshRenderer`，`SkinnedMeshRenderer` 不在实例集合里。
- **程序化/动画网格的 BVH 更新未支持**：官方建议用渲染层遮罩排除，或用代理几何体代替。
- **反射探针不捕获 SCGI 的间接光**：SCGI 只写屏幕辐照度纹理，不参与反射探针的渲染。
- **需要关闭 Static Batching，并把 Default Mesh Buffer Target 设为 Raw**：前者因为静态合批后拿不到原始网格，后者因为要直接读 mesh buffer 建 BVH。
- **改分辨率或级联数会重建缓存**：帧号归零，需要重新预热；改体积大小不会。
- **每个相机一份缓存**：多相机场景要按相机数乘显存。

## 来源

- 本机源码：`F:\Program Files\Unity\7000.0.0a9\Editor\Data\Resources\PackageManager\BuiltInPackages\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\` 与 `...\com.unity.render-pipelines.universal\Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\`
- 同版本的核心包还包含 `Runtime\PathTracing\MaterialPool\` 与 `Runtime\UnifiedRayTracing\`，分别对应材质转换和光追后端
- GitHub 同构源码：[core/Runtime/Lighting/SurfaceCache](https://github.com/Unity-Technologies/Graphics/tree/6000.7/staging/Packages/com.unity.render-pipelines.core/Runtime/Lighting/SurfaceCache)、[URP/SurfaceCacheGIRendererFeature](https://github.com/Unity-Technologies/Graphics/tree/6000.7/staging/Packages/com.unity.render-pipelines.universal/Runtime/RendererFeatures/SurfaceCacheGIRendererFeature)
- 官方文档：[SCGI 总览](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi.html)、[启用步骤](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-set-up.html)、[质量与性能调优](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-tune.html)
- 背景与定位见同目录的 [Unity SCGI 调研](Unity%20SCGI%20调研.md)
