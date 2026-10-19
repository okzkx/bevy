# Unity SCGI 调研：Surface Cache GI 与世界空间辐射缓存

## 0. 判定线

- **SCGI = Surface Cache GI**，不是 Screen-space 也不是任何 shader 配置缩写。Unity 光照团队（rasmusn）2026-05-20 公开预览，目标随 6.7 LTS 发布，URP 先行。
- **SCGI 是 diffuse-only 的动态 GI**：镜面间接明确交给 Reflection Probes / 即将到来的 SSR，不要指望它给反射。
- **机制底座 = "世界空间表面缓存 + 少量射线维护缓存 + 查询时收敛"**，官方自认灵感来自 Frostbite GIBS（surfels）、Kajiya 的 irradiance cache、NVIDIA SHARC（hash grid radiance cache），且是三者的组合而非直接实现。
- **射线预算花在缓存补丁上，不是每像素**：官方明确"每 Surface Cache patch 取 path tracing 样本（压低射线数）"，所以 DLSS/FSR 这类"屏幕空间逐像素路径样本"的重建器帮不上它的噪声。
- 底层走 UnifiedRayTracing 库：有硬件 RT 就用，没有就 compute shader 软件求交 fallback；Unity 自己不支持 Metal/Vulkan 硬件光追，SCGI 在这两个 API 上也只能走软件路线。

## 1. SCGI 是什么

来源：[Surface Cache GI Preview - Unity Discussions](https://discussions.unity.com/t/surface-cache-gi-preview/1720494)（官方预览帖，下面所有官方说法出自此帖及其 Staff 回复，除非另注）。

Unity 光照团队对 SCGI 的定位是"动态世界里的全局光照"，解决三件烘焙路线做不到的事：

1. **动态几何**：可破坏物、程序化世界、城建造物、开关门、动态加载场景——间接光照随几何变化实时适应。
2. **动态光照**：昼夜循环、开关灯、火把/手电——现有 Enlighten 和 APV Lighting Scenarios 虽然支持动态间接光，但都要预计算且只支持静态几何，SCGI 两个限制都解除。
3. **免烘焙迭代**：不用等 bake。官方甚至明说：发布时可切回烘焙拿最优性能，开发期用 SCGI 换取迭代速度。

设计目标的排序值得注意：**平台覆盖、运行性能、易用、健壮性优先于高端画质**。这是移动优先引擎的取舍，和 Kajiya 这类玩具渲染器"学习优先"的取舍恰好相反。

## 2. 从官方答疑拼出的运行模型

预览帖未公开完整技术文档（Getting Started 是 Google Doc，国内不可达），但 Staff 在帖内答疑透露了足够多结构信息：

- **覆盖范围靠 Surface Cache Volume 定义**："与 shadow map 类似，我们要求用户定义 SCGI 要覆盖的体块"。Volume 里配置球体半径/尺寸，无固定上限，但**半径越大越要配级联数**，避免远处堆太多精细 patch。
- **级联 + patch**：表面被网格化为 patch，"Cell Index"调试模式能看到 patch 随配置实时移动。patch 走 **ring buffer，有硬编码的"温和"预算**，超预算（大范围×高分辨率）会报容量问题；官方说后续可能开放预算给不在乎弱设备的用户。
- **每个 patch 是射线的发射点**：raygen 对 patch 发射线做直接光采样，多弹射靠缓存自反馈（见 SHARC/irradiance cache 的通用模式）。
- **半径外计划用"全局实时探针"兜底**（6.7 前加入）：探针随环境光实时变化但**不发射线、不产生遮挡**，官方建议用 SSAO 伪造遮挡。
- **自发光三角形支持弱**（时间上不稳定），计划加自发光三角形重要性采样解决；短期建议用点光源直接光 + SCGI 间接光组合。
- **依赖 Motion Vectors**：Scene View 没有 motion vectors 导致 Scene View 伪影多，说明系统按"表面随时间运动"建模，靠重投影稳定。
- **天幕/背景几何会挡住环境光的射线**（已知问题），计划加对象排除功能。
- 响应速度受 Temporal Smoothing 控制：置 0 反应最快但时域噪声大，加大样本数补偿。
- 已知缺陷清单里有趣的一条：**与 x/y/z 轴完全平齐的平面会闪**（z-fighting 样的时域闪烁），根因是轴对齐几何在缓存结构里的退化。

这个拼图足够画出架构：**球体覆盖 → 级联表面 patch 池（ring buffer 预算）→ patch 采样射线 → 世界空间缓存 → 屏幕空间像素查询**——与三大灵感来源的范式一致。

## 3. 三大灵感来源

### 3.1 Frostbite GIBS（surfels）

[GIBS: Global Illumination Based on Surfels - EA SEED（SIGGRAPH 2021）](https://www.ea.com/seed/news/siggraph21-global-illumination-surfels)

Frostbite 把表面离散成 **surfel（带半径的表面小圆盘）池**：从 G-buffer 足迹/射线命中处投放 surfel，每个 surfel 缓存辐出度（含入射方向分布），按需更新、过期回收。查询时命中点找邻近 surfel 汇聚即可得到间接光，多弹射通过"查询其他 surfel"自然产生。它是"表面贴纸式"的缓存——缓存贴着表面而不是体素填空间，天然回避了体积法的漏光问题，代价是 surfel 的生命周期管理与合并分裂（屏幕上细节变化时新建/回收）。

### 3.2 Kajiya 的 irradiance cache（机制最完整，值得精读）

[Kajiya GI overview - EmbarkStudios](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)（下述全部出自此文）

Kajiya 用一个**低分辨率辐照度缓存**给所有非首弹的射线提供"弹射尾巴"（单弹射太暗，完整 path tracing 太贵）：

- **数据结构**：12 张相机对齐的稀疏 clipmap，每级 32×32×32 体素，顶层是 32³ 的密集间接数组索引进预分配 65536 条目的 payload 池；条目按需分配、几帧不问即回收。
- **输出**：入射球面辐照度卷积成 **L1 球谐**存进条目，供其他 shader 查询。
- **产出（output-sensitive）的射线投放**：缓存从不被屏幕像素直接查询，而是被"别的射线打到的命中点"查询；每个查询点成为该体素的**投票候选**，每帧每体素等概率选一个投票点作为本轮射线的发射点。这个投票系统让缓存自动跟随用途——薄墙内外光照差异巨大的场景，相机在屋内候选点就在屋内，从根上抑制漏光。
- **Rank 系统**：缓存自反馈产生的多级弹射条目会投票出"外面视角"污染屋内查询，Kajiya 给每个条目记最低弹射级：屏幕射线看到的条目 rank 1，由 rank 1 条目繁殖的条目 rank 2……新投票只有 rank ≤ 旧值才生效，漏光消失。
- **法线偏置**：查询点沿表面法线偏移再查缓存，压制桌面类薄几何的双面漏光（本身也是折衷）。
- **每条目内**：4×4 八面体图分层的 ReSTIR reservoir，每帧 4 个格点出新候选 + 4 条验证射线；直接光太阳 + 间接光从缓存自喂（无双缓冲，竞态无害）；最后卷积成 L1 SH。
- 成本 ~0.55ms（RX 6800 XT, 1080p），响应快、对漏光健壮，但时域不稳、无空间平滑采样——Kajiya 文档自己把这两条列为缺点。

### 3.3 NVIDIA SHaRC（工程化最完整）

[SHARC - NVIDIA-RTX GitHub](https://github.com/NVIDIA-RTX/SHARC)（官方名 Spatially Hashed Radiance Cache，shader-only 源码随 RTXGI SDK 发布）

- **数据结构**：世界空间 **hash grid**，对数分级的体素尺寸（logarithm base 控制相邻层级体素比，sceneScale 控制平均体素大小，levelBias 控制近相机精细度），三条目等长 buffer（hash 表 / 帧内累计 / 跨帧 resolve），基线 2^22 条目，条目满了靠 hash 冲突 + stale 帧数驱逐。
- **三 pass**：Update（改良 path tracer 只跑一小撮像素，如 5×5 块取 1，约 4%，随帧铺满全屏；每段弹射独立入缓存）→ Resolve（compute 全量遍历：跨帧时域累计、过期驱逐、清帧内缓冲）→ Query（正式渲染时光线命中即查缓存，查到就提前终止路径）。
- **细节设计**：路径段短于体素尺寸要继续追踪（缓存粒度不够时不能停）；glossy lobe 只有锥展宽超过体素分辨率才可用缓存（给了锥宽估计式）；responsive lighting 模式让手电类瞬时光源快速生效；可选 SH 方向编码保留方向性避免镜面高光污染别的方向。

### 3.4 抽出共同范式

三者加 SCGI 是同一个思想的四种实现：

1. **屏幕空间样本变成世界空间缓存**：射线是稀缺资源，喂给缓存而不是直接喂像素。
2. **缓存按需生长**：surfels / 稀疏 clipmap 体素 / hash 条目都只在使用处分配，用 attendance（投票/命中）驱动。
3. **多弹射 = 缓存自查询**：缓存填充时的射线命中点继续查缓存，递归被摊平成迭代。
4. **时域收敛靠累计+验证**：累计指数平均（SHaRC 的 resolve、Kajiya 的 ReSTIR reservoir、SCGI 的 temporal smoothing）配"过期驱逐/样本验证"保证对变化响应。

## 4. 对 ash_renderer 的启示

- **可借鉴的第一步不是 SCGI 全体，而是"世界空间缓存"思想的最小件**：一组探针/体素存 L1 SH（数据量极小：L1 SH 每条目 12 个 half），一个 compute pass 按（相机跟随的稀疏网格 + 预算）更新，像素着色时查询。这与我们已有的 bindless 表 + compute 管线（5.2 剔除、5.3 命令环）形态完全兼容。
- **射线产生器是真正的新基建**：UnifiedRayTracing 的启示是"硬件 RT 与 compute 软件求交同一抽象"。无硬件光追时需要 software BVH（BLAS/TLAS 或简化为体素化场景/距离场），这是后续步骤评估的重点，SCGI 本身不解决这一层。
- **SCGI 的取舍句式可以照抄**：平台覆盖优先、diffuse-only、半径外探针兜底、自发光用点光组合——中型渲染器没有资源做全谱系，官方刀法就是参考答案。
- SCGI 的"屏幕空间噪声重建帮不上"提醒：世界空间缓存的噪声形态（patch 粒度）与屏幕空间 TAA/DLSS 假设（逐像素样本）不匹配，引入缓存路线时要同步设计自己的时域稳定手段。

## 来源清单

- [Surface Cache GI Preview - Unity Discussions](https://discussions.unity.com/t/surface-cache-gi-preview/1720494)（官方预览帖 + Staff 答疑，2026-05 起）
- [GIBS: Global Illumination Based on Surfels - EA SEED](https://www.ea.com/seed/news/siggraph21-global-illumination-surfels)
- [kajiya GI overview - EmbarkStudios](https://github.com/EmbarkStudios/kajiya/blob/main/docs/gi-overview.md)
- [NVIDIA SHaRC - GitHub](https://github.com/NVIDIA-RTX/SHARC)（含 Integration Guide）
