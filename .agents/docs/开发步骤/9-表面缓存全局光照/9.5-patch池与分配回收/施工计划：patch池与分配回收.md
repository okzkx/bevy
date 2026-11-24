# 施工计划：9.5 patch 池与分配回收

> 2026-11-24 立案（纯文档）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.5](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.5 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：⬜ 已立案（2026-11-24），未开工。** 任务 9.5.1～9.5.7 与判定线六条草案见下；前置闸门开工首日按序关闭。

**一句话说明**：把世界空间切成相机跟随的级联格子、每格 16 个方向桶、每 cell 一条 patch，patch 池定长环形复用——屏幕 1/4 播种按 G-buffer 反推可见表面来分配或刷新 patch，回收靠滚动/心跳/整理三件；本段只建池与生命周期（位置、法线、计数、回收），不出辐照度（9.6 采样）、不接屏幕注入（9.7），开关两种状态都不改画面。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-24 现状核实清单（本 checkout，逐项有出处）

| 项 | 结论 | 依据 |
|---|---|---|
| G-buffer 附件布局 | 槽 0 albedo / 1 normal / 2 **motion vectors（留位不分配）** / 3 matparam / 4 emissive + 深度；motion vectors 无内容、无 prev 变换链 | `passes/gbuffer/gbuffer_targets.rs:1-21` |
| G-buffer normal 语义 | 世界空间**着色法线**（normal map 经屏幕空间 TBN 扰动后），不是平面法线；播种的桶归属若直接用它，法线贴图细节会让同一平面上的桶来回跳 | `shaders/scene/gbuffer/gbuffer_write.wgsl:71-88` |
| 深度可采样（旧结论需订正） | 深度 usage = `DEPTH_STENCIL_ATTACHMENT｜SAMPLED`，合成已用 `textureSampleLevel(depth_tex, …)` 采样；**八段拆解"6.1 已实证深度附件不可采样→播种走 gbuffer_composite 通道"不成立**——那是 6.1 立案期（usage 只有附件位）的结论，Hi-Z 为采样扩了 SAMPLED 之后失效 | `core/target/depth.rs:42-53`、`shaders/scene/gbuffer/gbuffer_composite.wgsl:29/287`、`.agents/docs/笔记/可见性与GPU驱动/Hi-Z遮挡.md:85` |
| G-buffer 采样集阶段位 | `GBUFFER_SAMPLED_SET_DESC` 七条 binding 全是 `FRAGMENT`；compute 播种**不能**直接绑它（扩 stage 或另立集，二选一，见 §0.7） | `passes/gbuffer/gbuffer_targets.rs:57-71` |
| 播种插入点 | 必须排在 ④ G-buffer 之后：四色附件在 ⑤ 前转 `SHADER_READ_ONLY`（dst 已含 `COMPUTE`）、深度转 `DEPTH_READ_ONLY`；② 段时本帧 G-buffer 根本还没写 | `render/frame/record.rs:219-303` |
| 世界位置反投影 | `world_from_depth(uv, depth)` 目前是合成 shader 内的局部函数（`frame.inv_view_proj` 反投影），未进公共片段；播种要同一份数学 | `shaders/scene/gbuffer/gbuffer_composite.wgsl:231-237`、`shaders/common/frame.wgsl:17-19` |
| 帧 UBO | 336B、字段已排满到 320；set1 stage 含 `COMPUTE`（可被 compute 读），但**没有**分辨率/上一帧矩阵/GI 参数位 | `resident/tables/frame_uniform_table.rs:39/135-156`、`shaders/common/frame.wgsl:1-35` |
| 模型环与"上一帧" | 模型环是 HOST_VISIBLE 的 2 帧槽环、每帧增量写脏行 → **另一个帧槽就是上一帧的物体变换**（未脏的行两槽同值），物体级 prev 变换不必新开环 | `resident/tables/instance_table.rs:9/343/668-669`、`driver/host/frame.rs:186-188` |
| 相机上一帧 | 帧 UBO 只有当前 `view_proj`/`inv_view_proj`，没有 prev；种子重投影若要相机运动补偿，得自己存一份 | `frame_uniform_table.rs:110-133` |
| 帧槽数 | `MAX_FRAMES_IN_FLIGHT = 2`（"另一槽 = 上一帧"的推论靠它成立） | `core/device/context.rs:104` |
| GPU 时间戳 | 每帧槽 11 位已满（T0..T10，T9/T10 是 9.4 的单列两段）；新增 GI 段要扩 `QUERIES_PER_FRAME` + 新 point + 新 fetch 方法 | `stats/perf.rs:1-20/42/377-410` |
| compute 录制模式 | `ComputeHandles::record(device, cb, sets, push, groups, Handoff)` 是现成入口；新 set/管线必须在 `layout_registry` 登记（`every_shader_entry_has_a_pipeline` 与 BRP `layout_contract` 都读它） | `passes/ray_tracing/mod.rs:53-78/398-405`、`layout_registry.rs:1-5/50-110` |
| 求交器实际落点 | `render/ray_tracing/`、`passes/ray_tracing/`、`shaders/scene/ray_tracing/`——八段拆解 §5 写的 `render/rt/` 已随 9.4 落地改名，**新代码按现名引用** | `layout_registry.rs:29/82-83/109`、9.4 收官提交 |
| 桶索引 | `octahedral.wgsl` 已给 `direction_to_bucket_index(n, 4u)`（res=4 → 16 桶）与 CPU 镜像，cell 的桶号直接复用，不另写 | `shaders/common/octahedral.wgsl:29-41`、`passes/ibl/octahedral.rs` |
| 内存角色 | `BufferRole` 已有 `Storage`（HOST_VISIBLE 环）/`DeviceStorage`/`FrameUniform` 等；本段不新增角色，按"写一次初值 + 之后 GPU 读写"挑现成的 | `core/device/memory/role.rs:20-45` |
| BRP 方法族形状 | `remote/features/<域>.rs` 的 `#[system]` + `remote/mod.rs::with_method_main` 注册 + `tools/brp.py` 子命令；读回走 `readback_buffer`/`OneShotReadback` | `remote/features/ray_tracing.rs:56-135/200-286`、`remote/mod.rs:161-166` |
| Unity 源码在位与常量 | SCGI 主体在 `…\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\`；`patchCapacity = 65536`、`volumeAngularResolution = 4`（16 桶）、`lowResScreenScaling = 4`、`evictionThreshold = 60×4 = 240` 帧、`updateMax = 32`、`cascadeMax = 8` | `SurfaceCache/Common.hlsl:9-17`、`PatchUtil.hlsl:41-46` |
| Unity 播种式 | URP `PatchAllocation.compute`：1/4 网格全像素播种 + 每帧 `RandomPermute` 打散处理序；`FullResPixelOffset = (frame×7 % 16)/4, %4`（4×4 子像素偏移、16 帧一轮）；**cell 命中 → 刷几何 + heartbeat + rank 强制 0、辐照度历史保留**；新分配 → 种子走 MV 重投影 + 平面距离 < voxelSize 校验 + `updateCount = updateMax/2`；池满只计失败、不增长 | URP `PatchAllocation.compute:93-203`、`SurfaceCacheGIRendererFeature.cs:831-832` |
| Unity 回收式 | scrolling = 偏移变化后按 Res³×C 遍历出界 cell 三连释放；eviction = 容量线程、超 240 帧无心跳淘汰（有 bounce 先降 rank 1）；defrag = wave 前缀和压缩 + 读/写两份 ring 配置 flip-flop、每帧可配 1/2/4/8/16 槽 | `Scrolling.compute:19-56`、`Eviction.compute:17-50`、`Defrag.compute:52-98`、`SurfaceCache.cs:609-660` |
| Unity 的 L1 分量序 | `RGBL1 = { float3 l0; float3 l1s[3]; }`，求值是 `l1s[0]·dir.y + l1s[1]·dir.z + l1s[2]·dir.x`（y/z/x）；本工程 9.3 已统一成 x/y/z，patch 辐照度缓冲沿用**本工程**口径 | `SurfaceCache/Common.hlsl:47-56`、`shaders/common/octahedral.wgsl:4-5`、`shaders/common/ibl.wgsl:74-81` |

### 0.1 Volume：球形判据 + 立方存储，焦点 = 相机位置

- 规格（对齐八段拆解 §1.3 起步档）：Res 32 / 级联 4 / Size 128 m → 最细体素 = `Size / (Res · 2^(C−1))` = 128/(32×8) = **0.5 m**（Unity 同式 `SurfaceCache.cs:142`）；第 i 级体素 = 0.5×2^i m。
- 判据取 Unity 同款：位置落在级联内 = 到 Volume 中心的距离小于 `voxelSize·(Res/2 − 0.5)`，即**球判据 + 立方存储**（`PatchUtil.hlsl:260-270`）——有效覆盖是内切球，角落的 cell 会被判出界而不分配，不另造盒判据。
- 焦点 = 主相机世界位置（`view.camera.pos`），**不做** Focus Target（八段拆解 §3 待决项裁决；第三人称跟随焦点的需求归[外置条目 12～16](../../外置拓展需求/外置拓展需求.md)）。
- 量化：级联偏移是**整数体素偏移**，storage space = `(posVolumeSpace + cascadeOffset) mod Res`（有符号模，`PatchUtil.hlsl:244-258`）。宿主每帧按 `targetPos / voxelSize` 取整算偏移，最细级 1 个体素 = 0.5 m 的抖动粒度——只随体素边界移动，相机亚体素游移不引发 cell 抖动。**确切取整式施工时对 Unity 的 Volume 更新逐行核对**（本文只定不变量：偏移只取整数、各级独立、storage 按 Res 取模）。
- cell 线性化：`cellIdx = cascadeBase + 16·(x·Res² + y·Res + z) + bucket`（`PatchUtil.hlsl:216-224`），桶序与 `octahedral.wgsl` 的行主序一致。

### 0.2 数据布局与池容量（裁定：起步 65536，2^18 待压测）

| 表 | 元素布局（本工程对齐口径） | 字节/元素 | 容量 | 显存 |
|---|---|---|---|---|
| `CellPatchIndices` | u32（`0xFFFF_FFFF` = 空） | 4 | Res³×16×4 = 2,097,152 | 8 MiB |
| `CellAllocationMarks` | u32（同帧同 cell 分配仲裁） | 4 | 同上 | 8 MiB |
| `PatchGeometry` | position vec4 + normal vec4 | 32 | 65536 | 2 MiB |
| `PatchIrradiance0` | l0 / l1x / l1y / l1z 各一个 vec4 | 64 | 65536 | 4 MiB |
| `PatchIrradiance1` | 同上（时域/滤波后一份，9.6 写入） | 64 | 65536 | 4 MiB |
| `PatchStatistics` | mean vec4 + variance vec4 + irradianceState u32 + requestState u32 | 32 | 65536 | 2 MiB |
| `PatchCellIndices` | u32（反向指针：patch → cell） | 4 | 65536 | 256 KiB |
| `RingConfig` | count/start/end × 2 份（读/写 flip-flop） | 32 | — | 256 B |
| 计数与统计 | allocated / failed / freed / watermark / defrag 计数 | 64 | — | 256 B |
| **小计** | | | | **≈28.5 MiB** |
| 调试图（开 GI 才建） | RGBA16F 全分辨率 | 8/px | 1920×1080 | ≈7.9 MiB |

- 与八段拆解 §1.3 的"12 MiB patch 池 + 8/16 MiB cell 双表"一致（12 MiB = 2+4+4+2，正好对上"65536 条 ≈ 12 MiB"）。6 GB 显存预算下仍属可忽略量级，但按八段拆解 §7 第 4 条**必须与贴图预算同场测**，不单独声称便宜。
- `PatchStatistics` 用**本工程的显式对齐**（32B），不照抄 Unity 的 `float3` 紧凑打包：WGSL storage 里 vec4f 对齐 16，紧凑 float3 会逼出 stride 计算与读改写风险。
- 池容量取 65536（Unity 默认、满足 `2^32 % capacity == 0` 的环形取模前提）；`2^18` 只在大世界档分配失败率超阈值时再提，不在本段预设。

### 0.3 分配路径：1/4 屏幕播种 + 4×4 子像素 16 帧轮转，池满不增长

- 播种网格 1/4 分辨率（`lowResScreenScaling = 4`）：每帧覆盖全部低分辨率像素，处理序用帧哈希打散（`RandomPermute` 同款意图：池将满时"哪些 cell 先拿到预算"不偏向屏幕某一角）；真实采样点在全分辨率 4×4 块内按 `frameIdx×7 % 16` 轮转，16 帧一轮把 16 个子位置各点一次。
- 每线程：全分辨率深度 → `world_from_depth` → 平面法线（§0.5）→ 级联解析（§0.1）→ 方向桶 → cell → 解析 patch：
  - **cell 已有 patch**：索引不变，刷新几何位置/法线、`rank` 强制 0、heartbeat 刷新、**辐照度历史保留**（Unity 同款 `PatchAllocation.compute:152-159`）；
  - **新 cell**：`CellAllocationMarks` 原子换入（同帧同 cell 只有一个线程能拿到分配权）→ ring `end++` → `count < capacity` 才成功；失败则回退 marks 并把失败计数 +1（**池满不增长，只计数**）。
- 同帧冲突语义照抄 Unity：`marks` 的 `InterlockedExchange(…, 1, old)` 拿到旧值 0 才有分配资格；ring 用 `end` 递增后对容量取模。
- 种子（本段先给 IBL 基线，见 §0.4）：新 patch 的 `l0 = ibl_diffuse(平面法线)`、`l1 = 0`、`mean = l0`、`variance = 0`、`updateCount = 0`。

### 0.4 种子：IBL 基线；motion vectors 本体本段不做

- **本段种子 = IBL 漫反射基线**：新 patch 直接以 `ibl_diffuse(n)`（9.2 的 SH-L2 或 9.3 的 16 桶）作为初始辐照度，`updateCount = 0`。理由：9.5 阶段**还没有**低分辨率辐照度生产者（那是 9.7），Unity 的"上一帧低分辨率辐照度 + MV 重投影"种子在这里无源可吃；用 IBL 基线当先验等价于"缓存没数据就等于环境光"，与 9.7 的"Volume 外回退 IBL"同口径，9.6 的首轮收敛也不会从全黑起步。
- **motion vectors 本体：本段不做**（八段拆解 §2 9.5 明确允许的降级分支——"允许降级为相机重投影种子，motion vectors 转 9.6 或外置，并把差异记录在段施工记录里"）。代价与收益对不上：
  - 代价：G-buffer 附件 2 要真分配并把 `colorAttachmentCount` 从 4 改成 5（VUID-06179 要求与管线声明一致）、VS 多输出 prev clip、要存 `prev_view_proj`；
  - 收益：本段为零——没有低分辨率辐照度图可重投影，MV 只能用来"决定种子落点"，而本段种子是常量 IBL。
- 去向：9.6（时域混合与 bounce 种子）按需接；接口在本段预留两处——GI UBO 留 `prev_view_proj` 字段位、"另一帧槽模型环 = 上一帧变换"的结论（§0.0）直接可用，接的时候只补附件与 VS 输出。差异写进段施工记录。

### 0.5 采集面：平面法线从深度重建（G-buffer normal 不作主口径）

- 播种要的是**几何面法线**（决定 patch 朝哪个桶、patch 面在哪），G-buffer normal 是贴着法线贴图抖动的着色法线——直接拿它选桶，同一块平墙上会出现按贴图细节跳变的桶条纹。
- Unity 用独立的 `FlatNormalResolution.compute` 从深度重建平面法线。本工程不另开 pass：播种本来就是 1/4 全屏 compute，**在该 compute 内用深度一阶差分重建平面法线**（1/4 网格 × 3 次邻域深度反投影 → 叉乘），并把"用 shading normal"留成 A/B 开关（判定线 ① 读图对照）。
- 与 9.7 的一致性：查询端若用不同法线来源，播种桶与查询桶会错位（播种进 A 桶、查询读 B 桶 → 缓存命中率凭空腰斩）；**9.7 必须用同一来源**，写进 §6 接口。

### 0.6 回收三件：滚动 → 心跳淘汰 → 整理，顺序在播种之前

- 顺序 = scrolling → eviction → defrag → **播种**（Unity 的 preparation 在 update/alloc 之前，`SurfaceCache.cs:609-628`）。先腾空间再分配，同帧被释放的槽可以被本帧播种复用。
- **Scrolling**：级联偏移变化的帧才录（偏移没变则整个 dispatch 无意义）；逐 cell 取旧偏移换算回 volume space，出界就三连释放：`marks = 0`、`patchCell = INVALID`、`cellPatch = INVALID`（`Scrolling.compute:39-54`）。
- **Eviction**：dispatch 容量线程，`当前帧 − heartbeat > 240` 帧就淘汰。首版只做"无 bounce 直接释放"分支；bounce 的"先降 rank 1 再淘汰"留给 9.6（接口先留 `bounce_enabled` 位）。
- **Defrag**：把 ring 内容区间里的空洞项前移压缩，四张 patch 表同步搬、`cellPatch` 反向指针改写、读/写两份 ring 配置 flip-flop（奇偶切换）。每帧槽数可配（默认 2，档位 2/4/8/16）。
- **不依赖 subgroup**：Unity 用 `Threading::Wave` 前缀和；本工程首版用 **workgroup 共享内存前缀和**（512 线程块）绕开 WGSL subgroup 扩展在有/无 `enable subgroups` 设备上的分裂（前置闸门 §3 第 5 项先跑通这个形状，subgroup 只作后置优化，不进判定线）。

### 0.7 调试与计量：独立 compute 调试图 + BRP 四口，不改合成 shader

- **不往 `gbuffer_composite.wgsl` 里塞 GI 逻辑**：本段没有辐照度可显示，任何注入都是"为了看而改画面"，会把"GI 关 = 9.3 逐位一致"的降级路径弄脏。调试图走独立 compute（`passes/gi/`），输出 RGBA16F 的图 + 统计缓冲。
- 子视图（push 选）：0 = patch 分布（按级联/桶着色，把 patch 位置投到屏幕上画点）、1 = cell 占用热图、2 = rank/heartbeat 分布、3 = 池水位条。
- BRP 四口：`gi_set`（开关 + Volume/池/播种参数 + 调试图子视图）、`gi_stats`（patch 数、分配失败、回收计数、池水位/峰值、cell 占用率、rank 分布、`gi_ms`）、`gi_dump`（调试图 → PPM，复用 `frame_dump` 的 P6 写法）、`gi_census`（cell 表与 patch 表前 N 条对照：`cell↔patch` 双向一致、位置落在所属体素内、法线在对应桶内）。
- 帧计时新增 `SurfaceCache` 段：`QUERIES_PER_FRAME` 11 → 12、`TimestampPoint::SurfaceCache = 11`、`fetch_gi`（照 9.4 `fetch_ray_tracing` 先例单列）。
- G-buffer 采样集的阶段位：首选**把 `GBUFFER_SAMPLED_SET_DESC` 的 binding stage 扩到 `FRAGMENT|COMPUTE`**（一份布局、一份采样集，compute 与合成共用；扩 stage 只放宽不改语义，管线在 init 一次性重建）。备选是另立 `gi_screen` 集，但要复制七条 binding——两案在 9.5.1 首日各验一条零 VUID 后取改动小的那条。

### 0.8 边界（本段不主张什么）

- **不改画面**：`gi_enabled = false`（默认）= 与 9.3 收官态逐位一致；开也只多几条 compute 与一张调试图，不接合成与透明。
- **不出辐照度**：patch 表里两份 RGBL1 存在，但只有 IBL 种子值；射线半球采样、直接光、多跳、时域混合全归 9.6。
- **不接屏幕**：屏幕查表、空间/时域滤波、双边上采样、Volume 内替换 IBL 漫反射全归 9.7。
- **不做**：bounce 分配请求列表（9.6）、多相机/多 Volume（一份缓存服务一个主相机）、Focus Target、Unity 的 quasi-periodic 体素 warping（先记 §5，出现地板/墙面对齐闪烁再引）、alpha 裁切、AS defrag 式的 GPU 排序替代、cell 表压缩/稀疏化（SVO/hash 化）——这些要么是 9.8 的局限对照项，要么是外置条目。
- **不碰**：绘制闭环（draw/indirect/剔除）、IBL/天空链、4.x 的增量与回收账面（GI 表自建、不进资产账本，但**显存与分配失败必须进 BRP 读数**）。

## 1. 数据流总览

```
宿主（draw_frame，wait_for_slot 之后）：
  Volume 跟随：相机位置 → 各级整数体素偏移（量化 snap）
  写本槽 GI UBO：volume 参数 / 各级偏移 / 池与阈值 / frameIdx / prev_view_proj（9.6 用）
  建/换 GI 描述符集（首次 + resize）

录像（④ G-buffer 之后、⑤ 合成之前，新增 ④.5 段）：
  ① scrolling.wgsl   仅偏移变化帧：出界 cell 三连释放
  ② eviction.wgsl    容量线程：心跳超 240 帧淘汰（首版无 bounce）
  ③ defrag.wgsl      workgroup 前缀和压缩；读 ring 配置 A 写 B
  ④ patch_alloc.wgsl 1/4 播种：深度 → 平面法线 → 级联/桶/cell → 分配或刷新
  ⑤ gi_debug.wgsl    调试图（子视图 0..3）

BRP：gi_set / gi_stats / gi_dump / gi_census
```

## 2. 任务拆解

### 9.5.1 Volume 与格子系统（含 CPU 镜像）

**做什么**

- `shaders/common/gi_cell.wgsl`（公共片段，只放纯函数，`#import octahedral`）：`voxel_size(voxel_min, cascade)`、`is_inside_cascade(target, p, voxel, res)`、`resolve_volume_position(...) -> VolumePos {cascade, pos_volume, valid}`、`volume_to_storage(pos, offset, res)`、`cell_index(cascade, storage, bucket, res)`、`solve_signed_mod`。
- CPU 镜像 `passes/gi/volume.rs`：同一组公式 + 单测（级联边界、模运算负输入、cell 线性化往返、桶中心方向 vs `octahedral.rs` 的镜像一致）。
- GI UBO（`SurfaceCacheUniforms`，每帧槽一份，HOST_VISIBLE，≈224B）：`prev_view_proj`（64B）+ volume 参数（targetPos、voxel_min、res、cascades）+ 各级偏移（`array<vec4i, 8>`，w 位留用）+ ring 读/写偏移 + 帧号 + 阈值（evict/updateMax/bounce）。宿主每帧整份写，写入点与 `frame_uniform_table.write_frame_ubo` 同处（`wait_for_slot` 之后）。
- 帧号：`frameIdx++`（u32，wrap 无碍——heartbeat 用 16 位模距离，Unity 同款 `PatchUtil.hlsl:95-104`）。

**落点**：`shaders/common/gi_cell.wgsl`、`src/render/passes/gi/{mod,volume}.rs`、`src/render/resident/tables/surface_cache.rs`（GI UBO + 表）、`src/driver/host/frame.rs`（跟随与写入）。

**坑 / 钉子**

- **WGSL 的整数取模不能拿来当"正模"**：`-1 % 32` 依赖符号语义，Unity 为此专门写了 `SignedIntegerModulo`。`gi_cell.wgsl` 必须自带 `pos_mod` 并在 CPU 镜像用同一实现（单测钉负输入）。
- `int3` 在 uniform 里没有紧凑布局：级联偏移用 `array<vec4i, 8>`（xyz + w 留用），不要指望 12B 步长。
- 体素尺寸公式与"Size"语义只留一处：宿主与 shader 都从 `Size/(Res·2^(C−1))` 派生，不要在两处各写一个 0.5。
- 级联偏移必须是**整数体素**且**单调**：取整式（floor / nearest）施工时对 Unity 的 Volume 更新逐行核对后钉死，单测钉"同一目标位置连续帧偏移不倒退、跨帧最多变化 1 个体素"。

### 9.5.2 常驻表与描述符集

**做什么**

- `SurfaceCacheTables`：§0.2 的九类 buffer + 计数；建表时一次写初值（`cellPatch = INVALID`、`patchCell = INVALID`、marks/counters = 0、ring config = 0），之后只由 GPU 读写。
- `GI_SET_DESC`（全 `COMPUTE`）：b0 cellPatchIndices / b1 cellMarks / b2 patchGeometry / b3 patchIrradiance0 / b4 patchIrradiance1 / b5 patchStatistics / b6 patchCellIndices / b7 ringConfig / b8 counters / b9 GI UBO；按帧槽各一套集（ring config 双份索引在 UBO 里给偏移，不必两套 buffer）。
- 登记：`layout_registry` 的 `ALL_SETS` 与新管线的 `ALL_PIPELINES` 各加条目；`layout_contract` BRP 自检覆盖。
- 显存读数：表字节进 `perf_stats` 的显存组（新分组 `gi`：cell 表 / patch 表 / 调试图）。

**落点**：`src/render/resident/tables/surface_cache.rs`、`src/render/layout_registry.rs`、`src/render/stats/perf.rs`。

**坑**

- 初值必须"一次性写满整个 buffer"，不能靠 GPU 侧清零 dispatch（零 size 的 dispatch 与未初始化读都会踩 VUID）。用 staging 或 HOST_VISIBLE 直写，在建表期完成。
- cell 表 8 MiB ×2 的初值写是启动成本（几十毫秒量级），要计时并进日志；如果超预算，改 `DeviceStorage` + 一次性 compute 填 INVALID（仍是一次性，不进每帧）。
- 容量与取模前提：`capacity` 必须是 2 的幂且整除 2³²（65536 / 2^18 都满足）；将来若改成非 2 的幂，`end % capacity` 与 `UINT_MAX` 复用前提一起要重新论证。

### 9.5.3 屏幕播种与分配（`patch_alloc.wgsl`）

**做什么**

- `shaders/scene/gi/patch_alloc.wgsl`（compute，1/4 网格）：`textureLoad` 取全分辨率深度与 normal → `world_from_depth` → 平面法线（3 tap 差分）→ 级联/桶/cell → 解析 patch → 写几何/统计/种子；分配失败原子计数。
- 采样点轮转：`lowres 像素 → 全分辨率块内偏移 (frame×7 % 16)/4, %4`；处理序用帧哈希 + permute（低差异、跨帧不重复偏向）。
- 种子：`ibl_diffuse(平面法线)`（9.2 SH-L2 或 9.3 16 桶，按 `ibl_mode`）+ `l1 = 0`；`updateCount = 0`。
- 录制输入（`GiAllocDraw`）：GI 集、G-buffer 集、IBL 集、push（低分辨率尺寸 / 全分辨率尺寸 / 偏移 / frameIdx / 子视图，32B 内），组数 = ceil(1/4 像素数 / 64)。
- 返回：`GiAllocDraw` 只在 `gi_enabled && 有相机` 时 Some；空场景（无相机/无 G-buffer）整段不录。

**落点**：`shaders/scene/gi/patch_alloc.wgsl`、`src/render/passes/gi/allocation.rs`、`shaders/common/gi_cell.wgsl`。

**坑 / 钉子**

- **必须 `textureLoad`（整数坐标），不要 `textureSampleLevel`**：D32 的线性过滤不可依赖，而且平均后的深度反投影出来是错的几何。
- 播种的每条写入都要"同帧可见"给后面的调试图与 9.6：段末补 compute→compute 屏障（`MemoryBarrierDesc`，参 9.4 的 TLAS 两条屏障写法）。
- 1/4 网格的像素中心要按 `(x+0.5)/lowres_w` 再乘回全分辨率，别把 1/4 坐标直接当 uv（Unity 用 `PixelPosToUvPos` 明确 +0.5）。
- 平面法线差分在深度不连续（前景/背景交界）处会算出荒谬法线：用 4 邻域里深度差超过阈值的 tap 剔除，全剔除则跳过该像素（不播种）。
- 池满的失败计数用原子加；BRP 读它判断"预算是否够"，判定线 ④ 依赖它。

### 9.5.4 回收三件

**做什么**

- `shaders/scene/gi/{scrolling,eviction,defrag}.wgsl` 三条 compute。
- scrolling：`Res×Res×(Res·C)` 线程；只在"本帧偏移 ≠ 上一帧偏移"时录（偏移比较在宿主，读本槽与另一槽的 GI UBO 即可，无需回读 GPU）。
- eviction：容量线程；`(frameIdx − heartbeat) mod 65536 > 240` 淘汰；`bounce_enabled` 位存在但首版恒 0。
- defrag：每帧 2 槽（可配），workgroup 前缀和压缩；`RingConfig[read]` → `RingConfig[write]`，写完后奇偶切换；四表同搬 + `cellPatch` 改写。
- 挂进 `passes/gi/allocation.rs` 的录制序列（scrolling → eviction → defrag → alloc），屏障由各段自己补。

**落点**：三条 shader + `passes/gi/allocation.rs`。

**坑**

- defrag 的"起点/终点环绕"是这套算法最容易写错的地方：Unity 用 `start != end || !IsPositionInUseAssumingStartNotEqualEnd(start−wave, start, patchIdx)` 三重判定排除跨边界搬运（`Defrag.compute:63`）。照抄这三条判定，不要自创。
- 同一帧 defrag 与 alloc 都写 patch 表 → 必须有 compute→compute 屏障；写 ring 配置的两份也要屏障（读的那份本帧还被 scrolling/eviction 读过）。
- eviction 的虚拟线程数 = 容量（65536）而不是活跃数：线程里先判 `ring` 是否在用，否则会去淘汰从未分配过的槽。
- 帧号回绕：heartbeat 是 16 位字段，距离要用"模 65536 的最短距"（Unity 同款），直接相减会在大约 18 分钟后把全场同时淘汰。

### 9.5.5 录制编排与开关

**做什么**

- `RenderFeatures` 新增 `gi: Option<GiFrameDraw>`；`record.rs` 在 ④ G-buffer 的只读屏障之后、合成之前插入 ④.5 段（GI 关 = 整段不录、零命令、零屏障），段界新时间戳点。
- `driver/host/frame.rs` 新增 `prepare_gi(...)`：读 `GiSettings`、算 Volume 跟随与偏移、写 GI UBO、按需建/换描述符集、组装 `GiFrameDraw`。
- `GiSettings` 资源（默认 `enabled: false`）+ BRP `gi_set`：开关、Size/Res/级联/池容量（只允许在关态改，改后重建表）、播种开关、调试图子视图。
- 调试图与统计缓冲（每帧槽）：随 swapchain 重建；关闭时不建（懒建，首次开 GI 时建）。

**落点**：`src/render/frame/render_features.rs`、`src/render/frame/record.rs`、`src/driver/host/frame.rs`、`src/render/passes/gi/mod.rs`、`src/remote/features/gi.rs`、`src/remote/mod.rs`、`src/driver/host/init.rs`。

**坑**

- 段的插入位置是**硬约束**：② 段读不到本帧 G-buffer；⑤ 之后注入才轮到 9.7。本段录在 ④.5 只是"取数正确"，不是为了显示。
- 关闭态必须与 9.3 逐位一致：不建调试图、不绑集、不录 dispatch；时间戳点照录（两个相邻 mark 之间 ≈ 0），既有读数口径不变。
- 新段会让 `QUERIES_PER_FRAME` 从 11 变 12：`GpuTimer::fetch` 的 9 项分段口径不变，新增 `fetch_gi` 单读一位（照 9.4 双段先例）。
- 关闭后重开（BRP 切）要幂等：表不重建、ring 配置保留；只有容量/分辨率变更才重建（重建前 `device_wait_idle`，与调试图重建同款）。

### 9.5.6 调试视图与 BRP 四口

**做什么**

- `shaders/scene/gi/gi_debug.wgsl`：读几张表（cell 表 / patch 表 / counters / ring 配置）+ 帧 UBO 的 `view_proj`（把 patch 位置投到屏幕），写调试图：
  - 0 = patch 分布（级联基色 × 桶微调；点画在 patch 屏幕位置）；
  - 1 = cell 占用热图（按 `cellPatch != INVALID` 的密度显示，1/4 栅格）；
  - 2 = rank / heartbeat 分布；
  - 3 = 池水位条 + 失败计数条（屏幕左上角画条）。
- BRP：`gi_set` / `gi_stats` / `gi_dump` / `gi_census`（语义见 §0.7）；`gi_stats` 里除读数还要给"本帧 dispatch 组数"与各段 `gi_ms`。
- `tools/brp.py` 加四个子命令，`discover` 自动可见。

**落点**：`shaders/scene/gi/gi_debug.wgsl`、`src/render/passes/gi/debug.rs`、`src/remote/features/gi.rs`、`src/remote/mod.rs`、`tools/brp.py`。

**坑**

- `gi_dump` 读的是"该帧槽上一次提交"的调试图（两帧内新鲜），与 `ray_tracing_dump` 同一时序契约。
- 调试图是 storage image，写前 UNDEFINED→GENERAL；`gi_census` 读的是存储 buffer，走 `readback_buffer` + 一次性命令，别混两条路径。
- `gi_stats` 在关态回 `enabled:false` + 全零，而不是报错（BRP 客户端脚本靠这个判分支）。

### 9.5.7 判定线与计量

**做什么**

- `tools/gi_test.py`（照 `ray_tracing_test.py` 骨架：自启宿主、等 BRP、逐项采证、退出码 0/1），六条判定线：
  1. **分布吻合**：Sponza 中庭机位，`gi_set enabled=true` → `gi_dump view=0` 落 PPM，与 `frame_dump` 的着色帧、normal 调试帧对照（读图：墙/地/柱分面落到对应 patch，无漂浮点、无整屏空白）；`gi_stats.patches > 0`、`cell 占用数 ≤ 16×活跃 patch`。
  2. **显存恒定**：连跑 ≥120 帧，`gi_stats` 的表字节读数与 `perf_stats` 的显存组不增；`pool_watermark ≤ capacity`。
  3. **跟随无残留**：`set_transform` 把一组实例推近/移开、`chunk_despawn` 删一块后，分布图对应区域变化、旧位置的点消失；`gi_census` 的 `cell↔patch` 双向一致（无悬空、无菱形指针）。
  4. **稳态平衡**：静止游历 ≥60 帧后 `allocs − frees ≈ 0`、`failed ≈ 0`、池水位稳定；defrag 前后 `gi_census` 的 patch 集合只搬家不丢失。
  5. **大世界档可游历**：`generate_chunks` 铺开 + 相机轨迹（Size 512 / 6 级联 / 池 2^18 档），滚动后旧 cell 释放、池不涨、`failed` 有界；读数成组记录（活跃 patch、cell 占用、失败、水位、`gi_ms`、显存）。
  6. **不破底座**：`gi_set enabled=false` 帧与 9.3 收官态 `frame_dump` 逐字节一致；五脚本回归（`hiz_test` / `scene_lod` / `scene_stream` / `scene_fine` / `scene_sponza`）零回退；`cargo test`、`clippy`、`fmt`、零 VUID、`WM_CLOSE` exit 0。
- 计量：`gi_ms`（④.5 段整段）、分配失败、池水位峰值、cell 占用率、每帧 dispatch 组数；与 9.8 压测同口径。

**落点**：`tools/gi_test.py`、`src/render/stats/perf.rs`、施工记录。

## 3. 前置闸门（开工首日按序关闭）

1. **最小播种 compute 打通取数**：在 ④ 之后读深度+normal 并写一条 patch，光线求交/IBL 全关的基线下零 VUID——顺带定"扩 `GBUFFER_SAMPLED_SET_DESC` 阶段位 vs 另立集"（两案各跑一次，取改动小且契约测试全过的那条）。
2. **cell/patch 双表同帧读写**：scrolling → eviction → defrag → alloc 四段连续录制，syncval 零报（8.1 的 WRITE_AFTER_WRITE 教训：布局/访问位漏一个就报）。
3. **空场景与无相机**：不 panic、不录 dispatch、`gi_stats` 返回 0。
4. **契约测试**：`every_shader_entry_has_a_pipeline`、`layout_contract` 通过（新 set / 新管线 / 新 shader 入口已登记）。
5. **defrag 前缀和形状**：workgroup 版先跑通（不依赖 subgroup 扩展），在 256/512 线程两种块大小下与 CPU 参照逐个 idx 对账。

## 4. 显存与性能口径

- 静态显存：§0.2 的 ≈28.5 MiB（起步档）+ 调试图 7.9 MiB；大世界档 cell 表 12 MiB/张、池 2^18 时 patch 表 48 MiB。**逐项进 BRP 显存组**，并与贴图预算同场测（八段拆解 §7 第 4 条）。
- 每帧成本：scrolling（仅偏移变化帧，Res³×C ≈ 13 万线程）+ eviction（65536 线程）+ defrag（2×512 线程）+ alloc（1/4 像素数，1080p 下 ≈13 万线程）+ 调试图（开子视图才有）。量级估 0.1～0.3 ms，**实测为准**，`gi_ms` 单列进帧计时账。
- 分配失败的读数比毫秒更重要：失败率 > 0.1% 说明池/分辨率配比不对，先调参数再谈性能。

## 5. 待决项（施工时钉明，不预设结论）

| 待决 | 选项 | 裁决点 |
|---|---|---|
| 法线来源 | 深度差分平面法线 / G-buffer 着色法线 | 9.5.3 判定线 ① 的 A/B 读图；默认平面法线 |
| 播种网格 | 1/4 固定 / 1/2 或自适应 | 首版 1/4；覆盖率不足（失败率高但池未满）再议 |
| 体素 warping | 不做 / Unity 式 quasi-periodic warping | 地板/平墙出现 cell 归属闪烁时再引（`PatchUtil.hlsl:173-198`） |
| defrag 频率 | 每帧 2 / 4 / 8 / 16 槽 | 池水位与 `gi_ms` 实测后定，默认 2 |
| 池容量 | 65536 / 2^18 | 大世界档失败率 > 0.1% 才提 |
| cell 表内存角色 | DEVICE_LOCAL（一次性 compute 填初值）/ HOST_VISIBLE | 初值写耗时实测；决定 9.5.2 |
| 时间戳扩展 | 扩 `QUERIES_PER_FRAME` 加 `SurfaceCache` 点位 / 复用 GBuffer→Lighting 区间 | 施工时按读数需要定（倾向新点位） |
| 调试图分辨率 | 全分辨率 / 1/2 | 判定线 ① 读图精度是下限 |

## 6. 与后续段的接口

- **9.6（irradiance 更新）**：直接吃本段的 patch 表（几何/统计/两份辐照度）、`rank`、heartbeat、分配失败计数；bounce 分配 = 新增"请求列表 + rank 1"两条 shader，**不改本段结构**（`bounce_enabled` 位与 eviction 的降级分支已留）。种子接口（相机重投影 + `prev_view_proj`）在本段留好字段位，9.6 接 MV 时只补 G-buffer 附件 2 与 VS 输出。
- **9.7（屏幕 gather 与注入）**：查表入口 = `volume → cell → bucket → patchIdx`（本段 `gi_cell.wgsl` 的函数就是它的一半）；**法线来源必须与播种同源**（否则桶错位），只读 `rank == 0`；Volume 外回退 9.3 的 GlobalProbe/SH-L2。
- **9.8（动态验收与压测）**：本段的显存、分配失败、池水位、cell 占用率进压测组；体素 warping、池容量、defrag 频率的最终裁决在这里，不在本段。
