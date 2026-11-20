# Unity SCGI 调研：Surface Cache GI 与世界空间辐射缓存

SCGI 的射线不从屏幕像素发出：世界空间被划分成相机跟随的级联格子，每个格子再按表面朝向分成 16 个方向桶，每个桶里放一条 patch；patch 记录一个代表表面点的位置、法线和该点的 L1 辐照度，射线从这些 patch 发出。全局射线预算摊在 patch 上，不按像素数走。所以 DLSS/FSR 这类假设"每像素一个样本"的重建器帮不上它的噪声；它也只做 diffuse，镜面间接明确交给 Reflection Probes / SSR。它的底子是"世界空间表面缓存 + 少量射线维护缓存 + 查询时收敛"——射线命中点从缓存取间接光，多弹射靠缓存自反馈，时域上靠累计收敛。

**SCGI = Surface Cache GI：射线只用于更新缓存里的 patch，不用于逐像素计算；镜面间接不归它管。**

## 1. SCGI 是什么

来源：[Surface Cache GI Preview - Unity Discussions](https://discussions.unity.com/t/surface-cache-gi-preview/1720494)（官方预览帖，2026-05-20 公开；下面未另注的官方说法出自此帖及其 Staff 回复）。

SCGI = Surface Cache GI，不是 Screen-space，也不是任何 shader 配置缩写。Unity 光照团队在 2026-05 公开预览，目标随 6.7 LTS 发布，URP 先行。它是 **diffuse-only 的动态 GI**：镜面间接明确交给 Reflection Probes / SSR，不要指望它给反射。

机制底座是"世界空间表面缓存 + 少量射线维护缓存 + 查询时收敛"，官方自认灵感来自 Frostbite GIBS 的 surfel（带半径的表面小圆盘，缓存表面辐出度）、Kajiya 的 irradiance cache（按空间格子缓存入射辐照度，供别的射线查询）、NVIDIA SHARC（世界空间 hash grid 的辐射缓存），且是三者的组合而非直接实现。

Unity 光照团队对 SCGI 的定位是"动态世界里的全局光照"，解决三件烘焙路线做不到的事：

1. **动态几何**：可破坏物、程序化世界、城建造物、开关门、动态加载场景——间接光照随几何变化实时适应。
2. **动态光照**：昼夜循环、开关灯、火把/手电——现有 Enlighten 和 APV Lighting Scenarios 虽然支持动态间接光，但都要预计算且只支持静态几何，SCGI 两个限制都解除。
3. **免烘焙迭代**：不用等 bake。官方甚至明说：发布时可切回烘焙拿最优性能，开发期用 SCGI 换取迭代速度。

设计目标的排序值得注意：**平台覆盖、运行性能、易用、健壮性优先于高端画质**。这是移动优先引擎的取舍，和 Kajiya 这类玩具渲染器"学习优先"的取舍恰好相反。另一个同层级的取舍是平台：官方只支持 URP，HDRP 明确说"目前没有计划"；硬件光追只在 DX12、PS5、Xbox Series 上启用，Metal 和 Vulkan 上走 compute 软件光追回退。

## 2. 从官方文档与源码整理出的运行模型

预览帖时代只能靠 Staff 答疑整理结构；到 6.7 Beta，官方手册和源码都已经公开，下面这些点可以直接核实（源码级细节见同目录的 [Unity SCGI 实现](Unity%20SCGI%20实现.md)）：

- **覆盖范围靠 Surface Cache Volume 定义**：用户指定体积的 `Size`、`Resolution`、`CascadeCount`。体积没有固定上限，但覆盖越大越要靠级联数而不是加大分辨率，否则 patch 和格子都按立方增长。
- **级联 + patch**：世界空间被划分为级联格子，格子按方向分 16 个桶，每个桶最多一条 patch；patch 池是定长 65536 条，走环形缓冲。池满时分配失败、该格这一帧没有 patch，不会无限增长。官方预览期说的"ring buffer 有硬编码的温和预算"就是这条。
- **每个 patch 是射线的发射点**：每个 patch 每帧只取少量样本（默认 2 个，档位 1/2/4/8），多弹射靠缓存自反馈；新 patch 有预热倍率，加速收敛。
- **射线预算用于缓存 patch，不是每像素**：官方明确"每 Surface Cache patch 取 path tracing 样本（压低射线数）"，所以屏幕空间重建器帮不上它的噪声。
- **屏幕侧只做查询与滤波**：在 1/4 分辨率上查找世界缓存，再去噪、双边上采样，最后写出一张辐照度纹理供不透明物体着色。透明物体不在缓存里，退回环境探针。
- **体积外用全局实时探针兜底**：`DistanceFallback` 开启时，体积外的像素用一张由环境 cubemap 算出的 16 方向探针；它不发射线、不产生遮挡，官方建议用 SSAO 伪造遮挡。
- **依赖 Motion Vectors**：屏幕生成新 patch 时用运动矢量把上一帧的辐照度重投影过来做种子；Scene View 没有 motion vectors，所以预览期 Scene View 伪影多。
- **底层走 UnifiedRayTracing 库**：有硬件 RT 就用，没有就 compute shader 软件求交 fallback；硬件后端要求 DXR 的 inline ray tracing（`SystemInfo.supportsInlineRayTracing`），所以只在 D3D12 等平台成立。

官方手册现在的参数面比预览期清楚得多：`Quality` 预设 Low/Medium/High/Ultra 打包了体积分辨率、每帧样本数、预热倍率、时域平滑、空间滤波、屏幕查找数、去噪遍数和 defrag 数量；其余可单独调的是体积大小、级联数、距离兜底、patch warping、渲染层遮罩和强度。默认体积 128 米、4 级级联，Medium 档的格子约 0.5 米，和官方"patch 起步 0.25～0.5 米"的建议一致。

预览期列的已知缺陷，到 7.0 alpha 已经修了一部分：轴对齐平面闪烁用"量化前加世界坐标驱动的准周期波形偏移"缓解；自发光三角形有单独的列表和 MIS 采样；时域噪声多了独立的去噪 pass 和预热倍率；XR 和 double-sided GI 也进了 6.7 Beta。仍未解决的是 alpha cutout（6.7 按不透明处理，7.0 alpha 才加）、动画/程序化网格的 BVH 更新（roadmap 上，暂时建议用渲染层遮罩排除或用代理几何体）、以及反射探针不捕获 SCGI 贡献。

## 3. 发布状态与资料获取

截至 2026-11-19，SCGI **没有随任何正式版 Unity 发布**，但代码和文档都已经公开：

- 最新 6.7 Beta 是 **6000.7.0b9（2026-11-11）**，`6000.7.0f1` 尚不存在；当前稳定线是 6000.6.9f1。官方目标仍是 6.7 LTS，2026-11-06 已功能冻结。
- URP 包版本是 **17.7.0**，`CHANGELOG.md` 顶部还是 `[Unreleased]`，但已写入 "Surface Cache Global Illumination is now publicly available in URP"。
- Unity 7 Alpha（7000.0.0a16，2026-11-05）也带 SCGI，且比 6.7 多 cutout、Surface Cache Entity Culling、Material Resolution 等能力；a16 之前的 alpha（例如本机 a9）源码里查不到 cutout 和 Entity Culling 的实现。
- 官方文档在 6000.7 手册里已经成体系：[总览](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi.html)、[启用步骤](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-set-up.html)、[质量与性能调优](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-tune.html)、[自定义 Shader](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-custom-shader.html)、[排障](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-troubleshoot.html)、[Volume Override 参考](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi-volume-override-reference.html)。注意这些页面标题写的是 "Unity 6.7 LTS"，但版本选择器和 Release API 都表明 6.7 仍是 Beta，属于预发布文档。

代码不需要等正式版，两处都能拿到：

- **GitHub**：`Unity-Technologies/Graphics` 仓库的 `6000.7/staging` 分支，路径为 `Packages/com.unity.render-pipelines.core/Runtime/Lighting/SurfaceCache/` 和 `Packages/com.unity.render-pipelines.universal/Runtime/RendererFeatures/SurfaceCacheGIRendererFeature/`；`master` 分支同路径也在。
- **本机安装**：装 6000.7.0b9 或 7.0 alpha 后，源码在 `Editor/Data/Resources/PackageManager/BuiltInPackages/com.unity.render-pipelines.*` 下；如果项目走 PackageCache，则在 `Library/PackageCache/com.unity.render-pipelines.universal@17.7.0/`。本机现有 7000.0.0a9 和 `F:\okzkx\TestUnity7` 就是这条路径。

许可注意：这些包是 **Unity Companion License**，读、学、对照没问题；整段复制进自己的项目再分发有法律风险。**本项目执行期允许模仿/照抄源码用于本机学习（2026-11-20 拍板），公开分发前须替换为自写实现。**

## 4. 三大灵感来源

### 4.1 Frostbite GIBS（surfels）

[GIBS: Global Illumination Based on Surfels - EA SEED（SIGGRAPH 2021）](https://www.ea.com/seed/news/siggraph21-global-illumination-surfels)

Frostbite 把表面离散成 **surfel（带半径的表面小圆盘）池**：从 G-buffer 足迹/射线命中处投放 surfel，每个 surfel 缓存辐出度（含入射方向分布），按需更新、过期回收。查询时命中点找邻近 surfel 汇聚即可得到间接光，多弹射通过"查询其他 surfel"自然产生。它缓存的是表面上的点，而不是把整个空间体素化，天然回避了体积法的漏光问题，代价是 surfel 的生命周期管理与合并分裂（屏幕上细节变化时新建/回收）。

Unity 的 patch 是同一个思路的另一种组织：不存带半径的圆盘，而是用"体积格子 + 方向格"把表面位置量化成固定的 cell，每个 cell 对应一条 patch；生命周期用 rank 和 heartbeat 管，而不是 surfel 的合并分裂。

### 4.2 Kajiya 的 irradiance cache（机制最完整，值得精读）

[Kajiya GI overview - EmbarkStudios](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)（下述全部出自此文）

Kajiya 用一个**低分辨率辐照度缓存**给所有非首弹的射线提供后续弹射的近似（单弹射太暗，完整 path tracing 太贵）：

- **数据结构**：12 张相机对齐的稀疏 clipmap，每级 32×32×32 体素，顶层是 32³ 的密集间接数组索引进预分配 65536 条目的 payload 池；条目按需分配、几帧不问即回收。
- **输出**：入射球面辐照度卷积成 **L1 球谐**存进条目，供其他 shader 查询。
- **产出（output-sensitive）的射线投放**：缓存从不被屏幕像素直接查询，而是被"别的射线打到的命中点"查询；每个查询点成为该体素的**投票候选**，每帧每体素等概率选一个投票点作为本轮射线的发射点。这个投票系统让缓存自动跟随用途——薄墙内外光照差异巨大的场景，相机在屋内候选点就在屋内，从根上抑制漏光。
- **Rank 系统**：缓存自反馈产生的多级弹射条目会投票出"外面视角"污染屋内查询，Kajiya 给每个条目记最低弹射级：屏幕射线看到的条目 rank 1，由 rank 1 条目繁殖的条目 rank 2……新投票只有 rank ≤ 旧值才生效，漏光消失。
- **法线偏置**：查询点沿表面法线偏移再查缓存，压制桌面类薄几何的双面漏光（本身也是折衷）。
- **每条目内**：4×4 八面体图分层的 ReSTIR reservoir，每帧 4 个格点出新候选 + 4 条验证射线；直接光太阳 + 间接光由缓存自身提供（无双缓冲，竞态无害）；最后卷积成 L1 SH。
- 成本 ~0.55ms（RX 6800 XT, 1080p），响应快、对漏光健壮，但时域不稳、无空间平滑采样——Kajiya 文档自己把这两条列为缺点。

Unity 的实现和 Kajiya 最接近：同样是相机跟随的级联格子、同样存 L1 SH、同样用 rank 控制漏光。差别在细节——Unity 的格子是规则的三维网格加方向格（不是 hash grid），patch 池是定长环形缓冲（不是每帧投票选点），回收靠 240 帧心跳（不是几帧不问就回收）。

### 4.3 NVIDIA SHaRC（工程化最完整）

[SHARC - NVIDIA-RTX GitHub](https://github.com/NVIDIA-RTX/SHARC)（官方名 Spatially Hashed Radiance Cache，shader-only 源码随 RTXGI SDK 发布）

- **数据结构**：世界空间 **hash grid**，对数分级的体素尺寸（logarithm base 控制相邻层级体素比，sceneScale 控制平均体素大小，levelBias 控制近相机精细度），三条目等长 buffer（hash 表 / 帧内累计 / 跨帧 resolve），基线 2^22 条目，条目满了靠 hash 冲突 + stale 帧数驱逐。
- **三 pass**：Update（改良 path tracer 只跑一小撮像素，如 5×5 块取 1，约 4%，随帧铺满全屏；每段弹射独立入缓存）→ Resolve（compute 全量遍历：跨帧时域累计、过期驱逐、清帧内缓冲）→ Query（正式渲染时光线命中即查缓存，查到就提前终止路径）。
- **细节设计**：路径段短于体素尺寸要继续追踪（缓存粒度不够时不能停）；glossy lobe 只有锥展宽超过体素分辨率才可用缓存（给了锥宽估计式）；responsive lighting 模式让手电类瞬时光源快速生效；可选 SH 方向编码保留方向性避免镜面高光污染别的方向。

### 4.4 抽出共同范式

三者加 SCGI 是同一个思想的四种实现：

1. **屏幕空间样本变成世界空间缓存**：射线是稀缺资源，只用于更新缓存，不直接服务像素。
2. **缓存按需生长**：surfels / 稀疏 clipmap 体素 / hash 条目都只在使用处分配，用 attendance（投票/命中）驱动。
3. **多弹射 = 缓存自查询**：缓存填充时的射线命中点继续查缓存，递归被摊平成迭代。
4. **时域收敛靠累计+验证**：累计指数平均（SHaRC 的 resolve、Kajiya 的 ReSTIR reservoir、SCGI 的 temporal smoothing）配"过期驱逐/样本验证"保证对变化响应。

### 4.5 “空间网格缓存”与“表面元素池”是两条组织路线

前三节按灵感来源分，容易留下一个印象：SCGI 的 patch 就是 GIBS 的 surfel。源码给出的答案更准确：两者都是世界空间缓存，但离散的对象不同。

**表面元素池离散的是表面。** GIBS 从 G-buffer 足迹和射线命中处投放 surfel，每个 surfel 绑定一个表面元素，带半径；表面移动时 surfel 跟着移动、合并或分裂。查询时要在世界空间里找邻近的 surfel 做加权，所以需要空间加速结构，也要维护一套 surfel 的增删合并逻辑。缓存的大小随表面面积和细节增长，比网格格子更细的细节能保留下来。

**空间网格缓存离散的是空间。** 世界被切成格子，每个格子按方向分桶，每个桶最多一条缓存条目；条目里存一个代表表面点的位置、法线和辐照度。查询时用位置和法线算出格子地址，O(1) 取条目，不需要找邻居。缓存占用的内存在格子数和方向桶数确定后就固定，代价是格子尺度以下的细节丢失，同一格里多个表面要竞争同一个条目。

Unity 的 SCGI 采用第二种：

- 相机跟随的级联体素，默认最细一级约 0.5 米；
- 每个体素 16 个方向桶（4×4 八面体映射）；
- patch 池定长 65536 条，环形分配；
- 位置和法线每帧刷新为本帧的代表表面点，辐照度历史保留；
- 回收靠 240 帧 heartbeat，整理靠 defrag。

Kajiya 的 irradiance cache 和 SHARC 也在这条路线上：前者用相机对齐的 clipmap 体素，后者用世界空间 hash grid；差别主要在格子怎么组织、条目怎么选、怎么回收。GIBS 的 surfel 和它们不是同一类。

| 维度 | 表面元素池 | 空间网格缓存 |
|---|---|---|
| 离散对象 | 表面元素 | 空间格子 + 方向桶 |
| 条目身份 | surfel ID | cell + 方向 |
| 查询 | 邻域搜索或空间加速结构 | 位置算地址，O(1) |
| 内存 | 随表面面积和细节增长 | 由格子数决定，固定 |
| 格子尺度以下的细节 | 保留 | 丢失 |
| 表面移动 | surfel 跟随、合并、分裂 | 代表点刷新，条目不动 |
| 生命周期 | 按表面可见性增删 | heartbeat + 驱逐 |
| 典型实现 | GIBS | Kajiya、SHARC、Unity SCGI |
| 主要风险 | 邻域查询与生命周期管理 | 量化误差、多表面竞争、轴对齐闪烁 |

“Surface Cache”这个名字指的是缓存的内容——表面点的辐照度——而不是组织方式。Unity 的缓存内容对应表面点，地址对应空间格子。理解这一点，才能解释为什么 patch 的位置会每帧变化：位置是代表点，格子才是条目的身份。

对 ash_renderer 的选型含义是：实现世界空间缓存前，先决定离散什么。要固定射线预算、GPU 并行、O(1) 查询，空间网格更合适；要保留格子尺度以下的细节、能接受空间搜索和更复杂的生命周期，表面元素池更合适。两者也可以叠加，但那等于同时维护两套分配与回收逻辑，第一版代价过高。源码形态见同目录的 [Unity SCGI 实现](Unity%20SCGI%20实现.md)。

## 5. 对 ash_renderer 的启示

- **可借鉴的第一步不是 SCGI 全体，而是"世界空间缓存"思想的最小件**：一组探针/体素存 L1 SH（数据量极小：L1 SH 每条目 12 个 float），一个 compute pass 按（相机跟随的稀疏网格 + 预算）更新，像素着色时查询。这与我们已有的 bindless 表 + compute 管线（5.2 剔除、5.3 命令环）形态可复用；缓存数据布局与查询路径仍需新建。
- **射线产生器是真正的新基建**：UnifiedRayTracing 的启示是"硬件 RT 与 compute 软件求交同一抽象"。本机 RTX 2060 已核实支持 `VK_KHR_ray_query` / `VK_KHR_acceleration_structure` / `VK_KHR_ray_tracing_pipeline`，所以 ash_renderer 在 Vulkan 上可以直接走硬件 ray query；不想要硬件依赖时，Unity 的 Compute 后端（HLBVH + RestructureBvh + RadeonRays）是现成的软件路线参照，但实现量大得多。
- **SCGI 的取舍句式可以照抄**：平台覆盖优先、diffuse-only、半径外探针兜底、自发光用点光组合——中型渲染器没有资源做全谱系，官方取舍就是参考答案。
- **rank 与 heartbeat 是两个值得单独记的工程件**：rank 把"相机直接可见"和"射线间接可见"分开，屏幕查询只读前者，解决世界空间缓存最典型的漏光；heartbeat 用 16 位帧号做超时回收，比引用计数开销更低，也比每帧遍历池子开销更低。
- **SCGI 的"屏幕空间噪声重建帮不上"提醒**：世界空间缓存的噪声形态（patch 粒度）与屏幕空间 TAA/DLSS 假设（逐像素样本）不匹配，引入缓存路线时要同步设计自己的时域稳定手段。
- **显存要提前算**：Unity 的体积格子在高分辨率下是最大的一块（Resolution 64、4 级联时单张 cell 表 64MB），再叠加 BVH、材质图集和屏幕缓冲；6GB 的 2060 建议从 32/4 级联起步。

## 来源清单

- [Surface Cache GI Preview - Unity Discussions](https://discussions.unity.com/t/surface-cache-gi-preview/1720494)（官方预览帖 + Staff 答疑，2026-05 起）
- [Unity 6.7 手册：Surface Cache Global Illumination in URP](https://docs.unity3d.com/6000.7/Documentation/Manual/urp/surface-cache-gi.html) 及其子页面（Set up / Tune / Custom shader / Troubleshoot / Volume Override reference）
- [Unity-Technologies/Graphics：core SurfaceCache 源码](https://github.com/Unity-Technologies/Graphics/tree/6000.7/staging/Packages/com.unity.render-pipelines.core/Runtime/Lighting/SurfaceCache)
- [Unity-Technologies/Graphics：URP SurfaceCacheGI 源码](https://github.com/Unity-Technologies/Graphics/tree/6000.7/staging/Packages/com.unity.render-pipelines.universal/Runtime/RendererFeatures/SurfaceCacheGIRendererFeature)
- [GIBS: Global Illumination Based on Surfels - EA SEED](https://www.ea.com/seed/news/siggraph21-global-illumination-surfels)
- [kajiya GI overview - EmbarkStudios](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)
- [NVIDIA SHaRC - GitHub](https://github.com/NVIDIA-RTX/SHARC)（含 Integration Guide）
- 本机源码与实测：Unity 7000.0.0a9 内置包、`F:\okzkx\TestUnity7`、`vulkaninfo`（RTX 2060 的 ray query 扩展）
