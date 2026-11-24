# 施工计划：9.6 irradiance 更新

> 2026-11-24 立案（纯文档，未开工）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.6](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.6 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录（`9.6.K-*.md`），不回写本文。采样结构、时域数学与三路取光的 Unity 依据见[Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)§"射线怎么发"与 §"收敛怎么做的"，缓存存什么见[SCGI 间接光注入端](../../../笔记/光照与材质/SCGI间接光注入端.md)。
>
> **当前状态：✅ 收官（2026-11-24 开工、同日收官，临时分支 `goal/9.6`）。** 任务 9.6.1～9.6.7 为待施工拆解；前置闸门见 §3、判定线草案见 §9.6.7 与文末。一次只施工一段：按任务号推进，任务内实现、验证、记录后再进下一任务。实际做了什么见[段 README](README.md)与 `9.6.K-*.md` 施工记录。

**一句话说明**：每个活跃 patch 每帧发少量半球射线，命中点取"直接光（方向光阴影、光源侧点/聚光采样清单、自发光三角形）+ 邻近 cell 的缓存自查询"，投影成与 9.3 同口径的 RGBL1 辐照度并按方差引导做时域混合——多跳由缓存自查询自然形成；本段只把辐照度算进 patch 表、给出调试视图，不接屏幕查表/滤波/注入（归 9.7），`estimation` 关与 GI 关两种状态都不改画面。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-24 现状核实清单（本 checkout，逐项有出处）

| 项 | 结论 | 依据 |
|---|---|---|
| 缓存存什么 | patch 存**辐照度** E（RGBL1，系数已折余弦卷积与基常数，求值即 `l0 + l1·n`）；消费端再乘 `albedo·(1/π)`——所以命中点做多跳时要乘命中点 albedo、不再乘 π | `shaders/common/gi_cell.wgsl:34-40`、`shaders/common/ibl.wgsl:53-81`、`shaders/scene/gi/patch_alloc.wgsl:54-75`、`shaders/scene/gbuffer/gbuffer_composite.wgsl:273-275` |
| 9.5 留的挂点 | patch 统计已有 `state` 低 8 位 rank、`request`、`update_count`、`heartbeat`；GI UBO 的 `frame.z` = bounce 开关（9.5 恒 0）、`frame.w` = updateMax、`prev_view_proj` 只写不用 | `shaders/common/gi_cell.wgsl:42-54`、`gi_cell.wgsl:66-69`、`src/render/passes/gi/mod.rs:41-45` |
| 辐照度双缓冲 | 两份 RGBL1（GI 集 b3/b4）；新 patch 两份都写同一份 IBL 种子，所以 9.6 可直接用帧奇偶 ping-pong，不需要拷贝 | `src/render/resident/tables/surface_cache.rs:262-266`、`surface_cache.rs:425-426`、`patch_alloc.wgsl:192-198` |
| 阴影取数面 | `SHADOW_SET_DESC` 四张 cascade 图只声明 `FRAGMENT`，b0 均匀数据才含 `COMPUTE`；compute 里取 CSM **必须**扩 stage 或另立集 | `src/render/passes/shadow/shadow_map.rs:44-76` |
| 阴影采样函数位置 | `shadow_map_load` / `shadow_pcss` / `shadow_visibility` 是 `gbuffer_composite.wgsl` 的**局部函数**（PCSS 全价、带旋转核），不在公共片段 | `shaders/scene/gbuffer/gbuffer_composite.wgsl:92-183` |
| 光源取数面 | `LightRecord` 64B（pos+radius / color×intensity / flags），**没有** cone 外角；`LIGHT_CLUSTER_SET_DESC` 四条 binding 全是 `COMPUTE｜FRAGMENT`，光源常驻表 b1 在 compute 可读 | `src/render/resident/tables/light_table.rs:42-49`、`src/render/passes/lighting/cluster.rs:29-42` |
| 9.6 三件全新 | 光源侧采样清单（Unity `PunctualLightSampling.compute` 形状）、自发光三角形列表与面积 CDF、`QrngKronecker2D` 低差异序列——本 checkout 均无对应代码 | 在 `ash_renderer/src` 与 `ash_renderer/shaders` 检索 `QrngKronecker`、`emissive_triangl`、`punctual_light_sample` 三个关键词均无命中 |
| 材质环 emissive（八段拆解 §4 的"9.6 补列"已被提供） | 材质记录已含 `emissive: vec4f` 因子与 `tex_emissive` 贴图槽；G-buffer 已把"贴图 rgb × 因子"写进 emissive 附件——9.6 **不需要**扩材质环，只需自发光三角形列表 | `src/render/resident/tables/instance_table.rs:208`、`instance_table.rs:223`、`shaders/common/instance.wgsl:17-36`、`shaders/scene/gbuffer/gbuffer_write.wgsl:91-98` |
| 命中属性够用 | 命中可重建位置/法线/UV/材质环下标，`hit_world_pos` / `hit_world_normal` / `hit_uv` 已备；命中点 albedo 需采样 base color 贴图 | `shaders/common/raytracing.wgsl:31-103` |
| bindless 可进 compute | `BINDLESS_TEXTURE_SET_DESC` 两条 binding 都是 `COMPUTE｜VERTEX｜FRAGMENT`，compute 里可采样命中点 albedo 与 emissive | `src/render/resident/tables/bindless_texture_table.rs:43-48` |
| 求交集复用面 | 现 `RAY_TRACING_SET_DESC` 的 b3 是调试图 StorageImage，estimation 用不上——9.6 另立"TLAS + 顶点/索引池 + 统计"的求交集变体，不动 9.4 的那份 | `src/render/passes/ray_tracing/mod.rs:41-52` |
| 计数与计时 | GI 计数缓冲 16 字、已用 7（alloc / failed / freed / defrag_moved / view 三件），其余槽给射线数、命中数、bounce 请求、清单长度；`gpu_gi_ms` 已单列，`T5` = ④.5 表面缓存区间 | `surface_cache.rs:74-92`、`src/render/stats/perf.rs:103`、`perf.rs:15` |
| 淘汰侧已备 bounce 分支 | `frame.z != 0` 时先把 rank 0 降为 rank 1、刷新心跳、本轮不淘汰——9.6 打开 bounce 后不必改淘汰结构 | `shaders/scene/gi/eviction.wgsl:37-44` |
| 帧内顺序 | ② TLAS 构建 → ④ G-buffer → ④.5 表面缓存（回收三件 → 播种）→ ⑤ 合成 → ⑧ 视图；estimation 插在播种之后、合成之前 | `src/render/frame/record.rs:163-168`、`record.rs:286-295`、`src/render/frame/render_features.rs:46` |
| egui 面板已存在（9.6 只扩展） | `src/overlay/tool_window/gi.rs` 已有"表面缓存 GI"页：调试视图 ComboBox（`VIEWS` 1～8）、视图形态、法线来源、播种/整理参数，且"开 GI 而 view=0 时自动切 view=1"是面板便利默认；写的是同一份 `GiSettings`（BRP `gi_set` 同源） | `src/overlay/tool_window/gi.rs:3-8`、`gi.rs:13-30`、`gi.rs:37-60` |
| Unity 依据可本机对照 | SCGI 主体源码在本机（路径与许可边界见八段 §8.1），9.6 主要对照 `Estimation.compute`、`PunctualLightSampling.compute`、`EmissiveTriangleAddition/Removal.compute`、`EmissiveTriangles.hlsl`、`PathTracing.hlsl`、`Runtime/Sampling/QuasiRandom.hlsl` | 八段拆解 §8.1（2026-11-20 核实，2026-11-24 复核目录存在） |

### 0.1 采样形状：均匀半球 + QrngKronecker2D（订正八段"余弦加权"的说法）

- 照抄 Unity `Estimation.compute`：在 patch 法线周围**均匀**采半球方向，用 `uniformHemisphereDensity` 归一（`Estimation.compute:296-336`）。八段拆解 §2 9.6 写的"余弦加权半球采样"是立案期旧说法——改成均匀半球的原因是与 Unity 的 MIS 概率推导一致（半球命中自发光三角形的概率直接参与权重），余弦加权要重推这套权重，收益只是方差，代价是对照失效。
- 随机数：`QrngKronecker2D`（`Runtime/Sampling/QuasiRandom.hlsl`）以 `LowBiasHash32(patchIdx)` 与样本偏移初始化；样本偏移按帧递增（`sampleOffset = _SampleCount * (_FrameIdx + warmUpMultiplier)`，预热期是 `(_FrameIdx + 1) * _SampleCount`），保证同一 patch 不同帧的样本不重复——照抄 `Estimation.compute:380-393`。
- 本工程的低差异基建：`hammersley` / `radical_inverse_vdc` 已在 `shaders/common/ibl.wgsl:106-120`，但它们不是 Kronecker 2D；9.6.1 新写 `shaders/common/gi_sampling.wgsl` 放三件（Kronecker 2D、均匀半球、low-bias hash），不改 `ibl.wgsl`。

### 0.2 直接光三路：方向光阴影、光源侧点光清单、自发光三角形

三路取光全部照抄 Unity 的 `Estimation` 形状，各自独立随机序列、最后一起投影：

| 路 | 取数 | 成本 | 本工程前置 |
|---|---|---|---|
| 方向光 | 半球射线命中点算方向光可见性（低档 PCF：1～4 tap，**不跑 PCSS 全价**） | 每命中样本一次阴影采样 | 阴影集 stage 扩 `COMPUTE`（§3 闸门 2）；PCF 从 `gbuffer_composite.wgsl` 抽公共片段 |
| 点/聚光 | **光源侧预采样清单**：每帧 128 条射线从灯位向锥内采方向，记 `hitPos/hitNormal/hitAlbedo/distance/reciprocalDensity/lightIndex`；patch 侧每样本随机挑一条，打一条**重连接射线**验证遮挡与同一实例/三角形，再用 Jacobian 化简式算贡献 | 128 条/帧固定 + 每样本 1 条重连接射线 | `LightRecord` 无 cone 外角（`light_table.rs:42-49`）——聚光首版按点光近似并把锥角缺口记进 9.8 局限，或补字段（施工时二选一，见 §5） |
| 自发光 | 自发光三角形列表 + 面积 CDF：每样本按 CDF 抽三角形、面积采样打一条阴影射线；同时用半球采样打到同一三角形的概率做 MIS | 每样本 1 条阴影射线 | 材质环 emissive 已备（§0.0）；列表与 CDF 是 9.6 新建 |

- 光源清单的意义：**成本与 patch 数解耦**（128 条灯射线全场共享），这是 Unity 用清单而不是"逐 patch 逐灯打阴影射线"的原因（[Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md) §"点光采样"）。清单长度（128）与每帧条数作参数，进 `gi_set`。
- MIS 首版按 `PathTracing.hlsl` 的平衡启发式：半球命中自发光与面积采样两路并存时加权；出现双计先关面积采样那一路（A/B 记录），不靠调常数糊。
- 不做：透明自发光、alpha 裁切自发光（记 9.8 局限）；增量式三角形增删（首版每帧双缓冲重建）。

### 0.3 自发光首版：进首版（裁掉八段 §3 的"自发光首版"待决）

- 裁决：**进首版**。理由：材质环的因子与贴图槽已被提供（§0.0），需要的只是三角形列表 + CDF；Atrio 式"无 lightmap 场景靠自发光"的验收需要它。
- 范围限界：只收**不透明、非 alpha 裁切、emissive 因子或贴图非零**的在场实例三角形；每帧从实例表 + 网格池双缓冲重建，不做增量 `Addition/Removal`（Unity 有那对 compute，本工程首版省略，记录差异）。
- 强度：照 Unity 的全局 `_EmissiveTriangleIntensityMultiplier` 留一个乘数（进 `gi_set`），默认 1。

### 0.4 多跳与 bounce 分配：rank 0 才能分配，自查询读上一帧

- **自查询**：命中点在 Volume 内 → cell → 桶 → patch；有 patch 则取它的 E（上一帧那份）乘命中点 `albedo·(1/π)`；这就是多跳，`MultiBounce` 关 = 只剩直接光（A/B 用）。桶归属只能用命中重建给出的插值法线（`hit_world_normal`，`raytracing.wgsl:96-99`），而播种用的是深度差分平面法线——**两者在光滑曲面上不同源**，命中点查询会有桶错位（这是 9.5 §0.5"查询端与播种端法线必须同源"的已知例外）。9.6.3 要量化错位率（读图 + probe 对照），修法（补平面法线重建 / 按法线差回退相邻桶）留给实测后裁决。
- **读上一帧**：ping-pong 的写目标是 `frame_index & 1`，自查询读另一份。理由有二：避开同帧读写 hazard（9.5 的 syncval 教训），并保证多跳有确定的一帧传播序，不依赖线程调度。
- **bounce 分配**：命中点无 patch 时写分配请求；**只有 rank 0 的 patch 可以分配**（Unity `Estimation.compute:353-357` 的 `enablePatchAllocation`），每帧有请求上限；分配的 patch `rank = 1`。9.5 已把 `frame.z`（bounce 开关）与 `request` 字段留好，淘汰侧也留了"先降 rank 1 再淘汰"的分支（`eviction.wgsl:37-44`）。
- **rank 语义**：本段只**写** rank（bounce 分配置 1、屏幕播种回落 0）；"查询只读 rank 0"的强制在 9.7。不要把 rank 当"质量等级"用。

### 0.5 时域混合：照抄 Unity 的两段数学（累计平均 → 方差引导滞后）

- 数学照抄 `Estimation.compute:84-150`（`FilterTemporallyVarianceGuided` + `ProcessAndStoreRadianceSample`），要点：
  - `update_count < updateMax = 32` 时是**精确累计平均** `old / (old + new)`，不是指数混合——样本少时指数平均有偏；到 32 之后才转指数混合，所以"时间平滑度"只影响稳定后的行为。
  - 稳定后权重 = `lerp(0.995, short_hysteresis, driftWeight)`，`drift = |短期均值 − 旧 l0| / sqrt(方差)`，`driftWeight = smoothstep(0, 1, drift / 4)`；`short_hysteresis` 由配置映射到 0.75～0.95，默认值 9.6.4 实测后定。
  - 均值/方差只按短时滞后更新；每次更新计 `estimateUpdateCount = sample_count / _SampleCount` 次，所以预热倍率会一次性抬高 `update_count`。
- **种子会被首个估计整体替换**：`old_update_count == 0` 时短时权重为 0，输出直接是新估计（Unity 同款）。本工程的种子是 IBL 基线而不是低分辨率重投影，所以首帧噪声由预热倍率直接决定——预热倍率（1/2/4/8）与默认样本档（1/2/4/8，首版默认 2）一起进 9.6.4 的参数面。
- **`prev_view_proj` 不参与**：patch 身份跨帧固定，本设计没有重投影需求。保留字段，不用不删，施工记录里钉一句（否则后来者会以为漏了实现）。

### 0.6 射线预算与降采样：先写后测

- 硬约束：`活跃 patch 数 × 每样本射线数 × 样本档 ≤ budget`。每样本射线数 = 1（半球）+ 0/1（重连接）+ 0/1（自发光阴影），即最坏 3 条。
- 起步预算：**262144 射线/帧**（不是 65536×样本数——按活跃数算，广播种覆盖率下活跃数远小于容量）。超限按优先级降采样：rank 0 且心跳新的 patch 先保；其余按 `budget / (活跃数 × 3)` 换算样本档，最低 1；仍超则按固定稀释（每 N 帧更新一次，N 进参数）。
- 读数：`gi_stats` 扩 射线数 / 命中数 / bounce 请求数 / 实际样本档 / 平均方差 / 清单长度；不上"流畅"结论，成本进 §4 的组表。

### 0.7 开关与回退：三层各回上一层

- `estimation = false`（首版默认关；打开方式两条——egui 面板勾选（默认入口）或 BRP `gi_set`，两者写同一份 `GiSettings`）→ 不录 estimation 与清单 dispatch，patch 表只有 IBL 种子、`update_count` 不涨 → 与 **9.5 收官画面逐位一致**。
- 光线求交关 → estimation 不录（池与视图仍可开）；`ibl_enabled = false` → 种子走 ambient 常量分支（`patch_alloc.wgsl` 已有），9.6 不另造第三套。
- GI 关 → 与 9.3 逐位一致（9.5 已保证，本段不得破坏）。

### 0.8 边界（本段不主张什么）

- **不接屏幕**：1/4 查表、去噪、双边上采样、L1 压缩、Volume 内替换 IBL 全归 9.7。
- **不做 patch 级空间滤波与时域后滤波**：`SpatialFiltering.compute`（切平面抖动 taps、权重 `1 − |jitter|/√2`）与 `TemporalFiltering.compute`（`lerp(0.9, 0.95)`）归 9.7（八段 §2 9.7 明列）。本段只有 Estimation 内的方差引导时域混合——**这是对路线图 9.6 行"邻域空间滤波可关 / 空间滤波 A/B"的口径订正**，见文末说明。
- 不做：AO、镜面间接 / SSR / 局部反射探针；透明层不参与（9.2 的 IBL 继续管透明）；不改 CSM 覆盖范围（屏幕外漏光是已知局限，记 9.8）；不做 motion vectors 本体——9.5 §0.4 留的"转 9.6 或外置"支路在这里裁决为**外置**：patch 身份跨帧固定、种子来自 IBL，时域混合不需要 MV；9.7 的屏幕侧若需要再单独立案。不做烘焙、不做路径追踪。

## 1. 数据流总览

每帧（④.5 段，播种之后、合成之前）两次 dispatch + 一次可选视图：

| 步 | dispatch | 输入 | 输出 |
|---|---|---|---|
| ① 光源侧采样清单 | 清单条数（默认 128）个线程 | TLAS/顶点/索引池、光源常驻表、帧号 | 清单（hitPos/hitNormal/hitAlbedo/distance/密度/lightIndex）+ 计数 |
| ② 自发光三角形列表 | 在场实例/材质环 → 三角形（每帧重建，双缓冲） | 实例表、网格池、CDF 前缀和 | 三角形列表 + 面积 CDF + 计数 |
| ③ estimation | 池容量个线程（线程内判 ring 活跃） | patch 几何/统计、TLAS/池、清单、三角形列表、光源表、阴影图（CS）、bindless、GI UBO | 写 ping-pong 的另一份辐照度 + mean/variance/update_count/rank/request |
| ④ 视图（可选） | 9.5 的覆盖图/方片 + 新子视图 | 同上 | LDR 调试图 |

- set 组合：`instance(0) + gi_ray(1) + gi(2) + light_clusters(3) + shadow(4) + bindless(5)`，6 集是 9.6 的最紧处（§3 闸门 3 先验）。
- 屏障：播种写完 → estimation 读（compute→compute）；estimation 写辐照度 → 视图读（compute→片元/顶点，沿用 9.5 段末屏障）；清单与三角形列表在 ① ② 之后各自加一道。
- 数学链：字节累积（RGBL1）→ 余弦卷积 → 时域混合 → 写回。Unity 只保留 L1（一条 patch 12 个 float），本工程沿用 `PatchIrradiance` 的 4×vec4 布局（w 留用）。

## 2. 任务拆解

### 9.6.1 采样基座：Kronecker 2D、均匀半球与 low-bias hash

**做什么**

- `shaders/common/gi_sampling.wgsl`：`QrngKronecker2D`（照抄 `QuasiRandom.hlsl` 的 R2 序列 + `Init(seed, offset)`）、`low_bias_hash_32`、均匀半球采样（含 `uniform_hemisphere_density` 归一常数）、余弦加权的对照实现（A/B 用，不默认开）。
- 单测/自检：同 seed 同 offset 的序列在 CPU 参照下逐项相等（WGSL 侧用一条调试 dispatch 回读前 64 个样本）；序列在 patch 间无可见相关性（直方图粗糙检查即可）。

**落点**：`shaders/common/gi_sampling.wgsl`（新）、`src/render/passes/gi/estimation.rs`（新，仅自检入口）。

**坑 / 钉子**

- Unity 的 `QrngKronecker2D` 是 2D 输出喂一次样本（方向 + 概率），不要自己发明 3D 序列。
- 预热期与稳态期的样本偏移公式不同（§0.1），写反会让预热样本与稳态样本重叠。

### 9.6.2 直接光取数：CSM 低档 PCF、光源侧清单与自发光三角形

**做什么**

- **CSM**：把 `gbuffer_composite.wgsl` 的 `shadow_map_load` / `shadow_cascade` 抽到 `shaders/common/shadow.wgsl`（合成端继续用同一定义），另加一个 `shadow_pcf_low`（1～4 tap，无旋转核、无 blocker 搜索），estimation 只用低档。阴影集 stage 扩 `COMPUTE` 或另立 compute 变体（§3 闸门 2 定案）。
- **点/聚光清单**：`shaders/scene/gi/light_sampling.wgsl` + `src/render/passes/gi/light_samples.rs`（常驻表：清单缓冲 + 计数 + 双缓冲）。射线从灯位向锥内采方向（`LightRecord` 无外角 → 首版按点光全向/聚光按缺省锥角，见 §5），命中读实例/材质环与 albedo，写清单条目。
- **自发光三角形**：`src/render/resident/tables/emissive_triangles.rs`（三角形记录 + CDF + 计数，双缓冲）；`shaders/scene/gi/emissive_build.wgsl` 每帧重建（筛 `tex_emissive != 0` 或 `emissive.rgb > 0` 的不透明实例，展开三角形算面积、前缀和 CDF）。
- 三路在 estimation 里的入口函数先各自独立（`sample_directional` / `sample_punctual_bounce` / `sample_emissive`），便于逐路 A/B。

**落点**：`shaders/common/shadow.wgsl`（新）、`shaders/scene/gi/{light_sampling,emissive_build}.wgsl`（新）、`src/render/passes/gi/light_samples.rs`（新）、`src/render/resident/tables/emissive_triangles.rs`（新）、`shaders/common/gi_emissive.wgsl`（新）、`src/render/layout_registry.rs`（新集/新管线登记）。

**坑 / 钉子**

- 阴影集扩 stage 会同时影响合成与剔除的既有契约测试——扩 stage 只是放宽，但 `layout_contract_tests` 里对 stage 位的断言要同步核对。
- CDF 前缀和按"每三角形一线程"并行：第一遍写每条三角形面积、第二遍前缀和扫一遍，**不要**在 estimation 线程里现场扫描全表（那是 O(三角形 × patch)）。三角形数上限先按 32768 条起，超限截断并计数，别静默丢。
- 聚光锥角缺口：`LightRecord` 目前只给 pos/radius/color/flags，聚光当点光会漏光（锥外也照亮）——首版按点光近似并记录，或补 `cone` 字段（§5 待决）。
- 自发光倍增系数只乘一次：材质贴图已在 G-buffer 里按因子乘过，列表里不要再叠一次因子（避免双计），只乘全局强度。

### 9.6.3 多跳与 bounce 分配

**做什么**

- 命中点 → cell → 桶 → patch 查询（复用 `gi_cell.wgsl` 的 `resolve_cascade` / `cell_index` / `direction_to_bucket_index`），读**上一帧**辐照度，乘命中点 `albedo·(1/π)` 与 `MultiBounce` 开关。
- 无 patch → 写分配请求（每 patch 一个 `request` 位 + 计数），每帧请求上限；`enablePatchAllocation = bounce && rank == 0`，分配出的 patch `rank = 1`。
- 请求的消费：9.5 的 `patch_alloc.wgsl` 只做屏幕播种；bounce 分配首版走**下一帧的屏幕播种之外的第二条小 dispatch**（照 Unity `PatchAllocationRequest.hlsl` 形状），不塞进播种 shader（避免一个 shader 两个职责、且播种是 1/4 屏幕网格而请求是池网格）。
- rank 写侧与 `frame.z` 开关接上（淘汰侧分支已备，`eviction.wgsl:37-44`）。

**落点**：`shaders/scene/gi/estimation.wgsl`（自查询部分）、`src/render/passes/gi/{estimation,allocation}.rs`、`shaders/scene/gi/bounce_alloc.wgsl`（新）、`shaders/common/gi_cell.wgsl`（如需补一个 `lookup_patch`）。

**坑 / 钉子**

- 自查询必须读另一份缓冲（§0.4），否则多跳会在同帧内自激、还会和写入抢地址。
- 命中点法线与播种不同源（§0.4）：插值法线在光滑曲面上会把查询送到相邻桶，多跳偏色可能来自这里而不是估计本身——判定线 ① 前先把"桶错位率"设一条 debug 读数（视图 13 现有 rank 视图，另加一条法线差色阶）。
- bounce 分配的 ring 分配要复用 9.5 的 `CellAllocationMarks` 仲裁，不能另开一条"绕过 marks"的分配路径——否则同帧同 cell 会双分配。
- 分配出的 rank 1 patch 也吃估计（Unity 不按 rank 跳过估计），但**不**参与分配（`enablePatchAllocation` 只给 rank 0）。

### 9.6.4 L1 投影、余弦卷积与方差引导时域混合

**做什么**

- 字节累积 → `ProjectAndAccumulate`（L1 投影）→ `CosineConvolve`（折成与 9.3 同口径的"已折余弦卷积 + 基常数"形式，参考 `ibl.wgsl:53-81` 与 `patch_alloc.wgsl:54-75` 的种子代码，**不重推**常数）→ `ProcessAndStoreRadianceSample` 的等价实现（累计平均 → 方差引导滞后）→ 写 ping-pong。
- `update_count` / `mean` / `variance` 按 `Estimation.compute:104-148` 更新；`update_max = 32` 与 `short_hysteresis` 进 GI UBO（`frame.w` 已留 updateMax，其余字段按 §4 扩 UBO）。
- 参数面：`samples`（1/2/4/8）、`warmup`（1/2/4/8）、`short_hysteresis`、`multi_bounce`、`budget`、`emissive_intensity` 进 `gi_set`/面板。

**落点**：`shaders/scene/gi/estimation.wgsl`（主体）、`shaders/common/gi_sampling.wgsl`（SH 投影/卷积若归公共）、`src/render/passes/gi/estimation.rs`、`src/render/resident/tables/surface_cache.rs`（GI UBO 扩字段 + 静态断言）、`src/driver/host/frame/gi.rs`（UBO 写入与 ping-pong 奇偶）、`src/remote/features/gi.rs`（新参数）。

**坑 / 钉子**

- GI UBO 现在 256B（`GiUniforms` 固定布局 + `GI_UBO_SIZE` 静态断言）；扩字段必须同步 Rust 的偏移断言与 WGSL 的 `GiUniforms`，并保证对齐（`vec4` 不跨 16B）。
- L1 归一化口径：种子是"已折"形式（`sh_coeffs[0] * 0.28209479`），投影若再乘一次 `π` 或 `0.28209` 会整段偏亮/偏暗——先拿 9.2 的 `sky_uniform` 常量天空做离线自检（常量天空下估计应等于 `πL`），再进画面。
- `prev_view_proj` 不参与（§0.5），别顺手接出重投影。

### 9.6.5 射线预算、降采样与优先级

**做什么**

- 每 patch 预算换算：`budget` / `活跃数 × 3` → 样本档（1/2/4/8，向上取整后再夹到下界 1），再按"rank 0 且心跳新"给优先级，不足的 patch 按帧稀释（`frame_index % N != patch_hash % N` 就跳过）。
- `gi_stats` 扩读数：射线数、命中数、bounce 请求、样本档分布、平均方差、清单长度、活跃 patch 数、`estimation_ms`。
- 收敛判据读数：`update_count` 到 32 的 patch 占比、方差均值随帧下降曲线（进判定线 ③）。

**落点**：`src/render/passes/gi/estimation.rs`、`shaders/scene/gi/estimation.wgsl`、`src/remote/features/gi.rs`（stats）、`src/render/stats/perf.rs`（`estimation` 计时区间，见 §5）。

**坑 / 钉子**

- 预算按"容量"算会在广播种场景下把帧预算打穿（活跃数远小于 65536，但播种刚过的帧活跃数可以很高）——必须先读活跃数（ring 水位），再决定样本档。
- 降采样不能引入"同一 patch 隔帧跳档"的闪烁：稀释用 patch 哈希取模，不要用 `patchIdx % N`（会按池顺序成块跳）。

### 9.6.6 调试视图、BRP 与 egui 面板

**做什么**

- 9.5 的覆盖图/方片加子视图：9 = 每 patch 样本档、10 = 射线数色阶、11 = 方差、12 = `update_count`、13 = rank 分布（9.5 的 4 号 rank 视图在 9.6 里才有非零值，直接复用其配色）。
- **egui 面板必须能直接开调试视图**（同 9.5 的口径，面板是默认入口、BRP 只是脚本化通道）：`src/overlay/tool_window/gi.rs` 的 `VIEWS` 表扩到 9～13（编号与 `gi_cell.wgsl` 的 `view_color` 同源，名称写清"样本档 / 射线数 / 方差 / 更新次数 / rank"），ComboBox 直接选；沿用 9.5 的便利默认——打开 `estimation` 而 `view == 0` 时自动切到 10（射线数），否则开了也看不到任何变化。
- 面板控件补齐 `estimation` 开关与 §9.6.4 的参数面：样本档、预热倍率、`multi_bounce`、`short_hysteresis`、`budget`、`emissive_intensity`、清单条数（滑条/开关，就地写 `GiSettings`，`draw_frame` 下一帧生效）；面板底部的 BRP 提示行补 `gi_estimate_probe`。
- BRP：`gi_set` 扩 `estimation` / `samples` / `warmup` / `short_hysteresis` / `multi_bounce` / `budget` / `emissive_intensity` / `light_samples`；`gi_stats` 扩 §9.6.5 的读数；`gi_estimate_probe`（新）：按坐标或 patch 索引回读单条 patch 的几何/统计/辐照度（判定线要"单点对账"，靠视图只能看颜色）。面板与 BRP 写同一份 `GiSettings`，手动切与 AI 调参不冲突。

**落点**：`shaders/scene/gi/patch_view.wgsl`、`src/render/passes/gi/{mod,view}.rs`、`src/remote/features/gi.rs`、`src/remote/mod.rs`、`src/overlay/tool_window/gi.rs`、`tools/brp.py`（若需注册新方法名）。

**坑 / 钉子**

- 视图号是脚本契约（`gi_test.py` 已用 1/3/7）；新增只往后加，不改旧号语义。
- 面板的 `VIEWS` 表与 `gi_cell.wgsl` 的 `view_color` 是两份手写编号，改一处忘另一处就会出现"选了视图但画面不变"——加一个 Rust 侧单测断言两侧编号集合相等（照 `layout_contract_tests` 的做法）。
- 面板控件只在 `GiSettings` 里加，不要在面板内另存一份 UI 状态，否则 BRP 改参后面板显示不回读。
- `gi_estimate_probe` 的读回要走已有 `readback` 通道，不要新开逐帧 `device_wait_idle`。

### 9.6.7 判定线与计量

**做什么**

- 新脚本 `tools/gi_estimation_test.py`（照 `ray_tracing_test.py`/`gi_test.py` 形状：自启宿主、逐项采证、退出码）+ 复用 `gi_dump` 的 PPM 读图；判定线七条：
  1. **多跳色渗可见**：红墙/绿顶/白球场景，`multi_bounce` 关/开 A/B 读图（色渗出现在开的那侧），并给出开关后的 `gi_stats` 对照。
  2. **开关灯 lag 量级如实记录**：点光开关后到 patch `update_count` 重新到 32 的帧数、以及 1/4 亮度恢复帧数；只记录，不承诺帧数。
  3. **收敛后预算可降档且画面稳定**：静置后把 `samples` 从 2 降到 1、`budget` 减半，画面差分在阈值内、方差读数不反弹。
  4. **自发光 A/B**：`emissive_intensity` 0 / 1 读图 + 单点 probe 对账（关闭时应只剩直接光与 IBL 种子）。
  5. **阴影漏光案例记录**：屏幕外/CSM 覆盖外的 patch 无方向光遮挡的漏光读图，留档给 9.8。
  6. **回退硬线**：`estimation=false` 的 LDR 帧与 9.5 收官待测帧逐字节一致；GI 关 = 9.3 逐位一致。
  7. **不破底座**：零 VUID/VK-ERROR、clippy 全净、`cargo fmt --check` 绿、库测试全过、`gi_test.py` 25 项不回退、五脚本回归（`hiz_test` / `scene_lod` / `scene_stream` / `scene_fine` / `scene_sponza`）全过。
  8. **面板路径可用**：不看 BRP 文档，仅用 egui 面板就能打开 9.6 的全部子视图、勾 `estimation`、改样本档与预算；同一组操作在 BRP 侧等价，判定线 ①～④ 的画面与读数在面板路径下可复现。
- 计量口径：按[八段拆解 §7](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md) —— 射线预算按 patch 不按像素、显存成组、动态行为如实记录、漏光留档、不破底座。

**落点**：`tools/gi_estimation_test.py`（新）、`src/render/stats/perf.rs`、施工记录。

**坑 / 钉子**

- "多跳可见"是最容易被单点误差糊过去的判定线：必须给出关/开两帧的同一机位读图 + 同一 patch 的 probe 数值，只给一张"看起来有色渗"的图不算过。
- 逐字节一致要在同一进程、同一帧槽、同一 `estimation` 前后状态下取，跨进程 PPM 比较会因驱动排序假失败（9.5 的教训：用 `gi_dump` 同一通道）。

## 3. 前置闸门（开工首日按序关闭）

1. **最小 estimation 打通**：单 patch 一条半球射线、只取方向光、零 VUID；`estimation=true` 时 `update_count` 开始涨、`gi_stats` 有射线读数。
2. **阴影集 stage 扩 `COMPUTE` vs 另立 compute 变体**：两案各跑一次零 VUID + `layout_contract_tests` 全过，取改动小的一条并记录（同 9.5 G-buffer stage 闸门的先例）。
3. **六集管线**：`instance + gi_ray + gi + light_clusters + shadow + bindless` 的最小空 dispatch 零 VUID；若布局契约/驱动在集数上卡住，优先把"清单 + 三角形列表"并进 GI 集（扩 b10/b11），再退回减集——记录最终形状。
4. **ping-pong 奇偶**：同一帧内 estimation 写 A、视图读 A、自查询读 B，三处一致；连跑 120 帧无"读到上一帧的上一帧"抖动。
5. **空场景 / 无灯 / 无光追 / 无自发光材质**：不 panic、不录对应 dispatch、`gi_stats` 回 0 形态（脚本靠这个判分支）。
6. **回退硬线**：`estimation=false` 与 9.5 收官帧逐字节一致；GI 关与 9.3 逐位一致。
7. **egui 面板能开全部视图**：不碰 BRP，在面板里能选到 9～13 五个子视图、勾 `estimation`、改样本档，画面与 `gi_stats` 读数随之变化；BRP 改参后面板显示同步（同一份 `GiSettings`，不回读旧值）。
8. **契约测试**：新 shader 入口都有管线（`every_shader_entry_has_a_pipeline`）、`layout_contract` 通过、新常驻表进显存组、`VIEWS` 与 `view_color` 编号集合一致。

## 4. 显存与性能口径

- 新增显存（起步档）：自发光三角形列表双缓冲（48B/三角形，上限 32768 条 → 3 MiB）+ CDF（4B/条 → 256 KiB）+ 光源清单（64B × 128 × 2 槽 → 16 KiB）+ 计数/UBO 扩字段；合计 < 4 MiB，与 9.5 的 ≈30.5 MiB 同场记录，并进 BRP 显存组与贴图预算同场测。
- 每帧成本（2060，Release，1080p，Sponza/zero_day）：清单 128 线程；三角形列表重建按在场三角形数（上限截断）；estimation = 池容量 65536 线程、活跃内才有工作，射线数 = 活跃 × 样本档 × 最坏 3。`estimation_ms` 单列进帧计时账，首版目标 ≤ 2 ms（**先写后测**，实测为准，不拿"流畅"替代读数）。
- 读数成组：活跃 patch 数、射线数、命中数、bounce 请求、样本档分布、平均方差、`update_count` 分布、`estimation_ms`、显存各项；与 9.5 的 alloc/failed/freed/池水位同场。
- 调参优先级：先把 `failed ≈ 0` 与 `update_count` 收敛拿到，再谈射线数与 ms——分配失败率高时降射线不会让画面变好。

## 5. 待决项（施工时钉明，不预设结论）

| 待决 | 选项 | 裁决点 |
|---|---|---|
| 计时点位 | 新增 `Estimation` 点位（12 → 13 位，按 9.5 先例重编号）/ 并入 `SurfaceCache`（T5）区间 | 9.6.5 首次读数需要把 estimation 与回收三件分开时再加点位；否则并入 |
| 样本档与预算默认 | 2 样本 / 262144 射线；或 1 样本 / 131072 | 9.6.5 的 1080p 读数与判定线 ③ 的降档稳定性 |
| `short_hysteresis` 默认 | 0.75 / 0.85 / 0.95 | 9.6.4 的开关灯 lag（判定线 ②）与稳态噪声读图 |
| 聚光锥角 | 首版按点光近似并记局限 / 给 `LightRecord` 补 cone 外角字段 | 验收场有聚光时看漏光读图；补字段会动光源表与分簇契约测试 |
| 清单长度与每帧条数 | 128（Unity 同款）/ 256 | 9.6.2 实测的清单命中率与 `estimation_ms` |
| 自发光三角形上限 | 32768 / 65536 / 不限 | 验收场实际三角形数与显存账 |
| MIS 是否保留 | 保留（按 `PathTracing.hlsl`）/ 关掉面积采样只留 trace 侧 | 判定线 ④ 是否双计、方差是否可接受 |
| 命中点法线口径 | 用插值法线直选桶（首版）/ 补命中点平面法线重建 / 按法线差回退相邻桶 | 9.6.3 的桶错位率读图与判定线 ①（多跳色渗）；光滑曲面上错位明显时再修 |
| rank 0→1 的触发 | 只由 bounce 分配置 1（屏幕播种回落 0）/ 另加降到 1 的规则 | 9.6.3 的请求上限与 9.7 的"只读 rank 0"命中率 |
| GI UBO 扩字段的打包 | 现有 256B 已排满（`prev_view_proj 64 + target_pos 16 + volume 16 + 级联偏移 128 + ring 16 + frame 16`），本段新增的样本档/预热/短滞后/预算/自发光强度/清单长度基本塞不下 → 扩到 272/288B（同时把 `ring.w` 这个空位用掉）/ 只扩一半、其余走 push 常量 | 9.6.4 施工首日定：扩字段要同步 Rust 偏移断言、WGSL `GiUniforms`、`GI_UBO_SIZE` 与 `gi_stats` 读数口径 |

## 6. 与前后段的接口

- **9.5（patch 池）**：直接吃 patch 几何/统计/两份辐照度、`rank`、`request`、`update_count`、ring 水位与分配失败计数；bounce 分配复用 `CellAllocationMarks` 仲裁（§9.6.3 坑），不改 9.5 结构。`frame.z`（bounce）与 `eviction` 的降级分支由本段打开。
- **9.7（屏幕 gather 与注入）**：屏幕侧只读 **rank 0** 的**当前**那份辐照度（奇偶由 GI UBO 暴露）；patch 级空间滤波与时域后滤波在 9.7 落地（本段边界 §0.8）；Volume 外回退 9.3 的 GlobalProbe/SH-L2。
- **9.8（动态验收与压测）**：本段的射线/样本/方差/清单长度、开关灯 lag、漏光案例（判定线 ②⑤）直接进压测与局限对照；聚光锥角、自发光上限、预算默认值的最终裁决在 9.8，不在本段。

## 判定线草案（段级汇总）

1. 多跳色渗可见（`multi_bounce` 关/开读图 + probe 数值对照）。
2. 开关灯 lag 量级如实记录（不承诺帧数）。
3. 收敛后射线预算可降档且画面稳定（读数 + 差分阈值）。
4. 自发光贡献开关 A/B 与单点对账。
5. 阴影漏光案例读图留档（CSM 只覆盖主相机视锥）。
6. 调试视图与参数在 egui 面板可直接操作（无 BRP 也能复现全部子视图与开关），面板与 BRP 同源。
7. 回退硬线：`estimation=false` = 9.5 收官逐字节一致；GI 关 = 9.3 逐位一致。
8. 零 VUID、clippy 全净、`cargo fmt --check` 绿、库测试与 `gi_test.py` 25 项不回退、五脚本回归全过。

> **口径订正说明（2026-11-24 立案）**：路线图步骤 9 表 9.6 行原写"邻域空间滤波可关 / 空间滤波 A/B"，八段拆解 §2 9.7 明列空间滤波与时域后滤波归 9.7。本计划按八段拆解分工（9.6 = Estimation 内的时域混合），路线图该行同步改为"时域累积在 patch 空间（方差引导）；patch 级空间/时域后滤波归 9.7"，以减少两处自相矛盾。
