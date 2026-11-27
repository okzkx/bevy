# 施工计划：10.2 屏幕空间环境光遮蔽（SSAO）

> 立案日期：2026-11-27（纯文档立案；零代码、未提交、未推送）
> 当前状态：✅ 收官（2026-11-27，判定线 6/7，第 4 条口径未达）。实际读数与逐条判定见[段 README](README.md)，施工记录 `10.2.1`–`10.2.6`。
> 上游文档：[步骤 10 README](../README.md)、[主计划：表现与使用体验优化三段拆解](../施工计划：表现与使用体验优化三段拆解.md)、[学习目标实现步骤](../../学习目标实现步骤.md)、[全局光照谱系调研](../../../笔记/光照与材质/全局光照谱系调研.md)、[Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)
> 文档分工：本文只写做什么、按什么顺序做、怎么验收；实际读数、截图与逐条判定另立 10.2.x 记录，不回写本文冒充已完成。

**一句话说明**：在 G-buffer 与合成之间加一条半分辨率屏幕空间环境光遮蔽支路（SSAO compute 写半分辨率 AO → 深度感知上采样成全分辨率 AO → 合成把 AO 乘到环境项），给环境光照补上接触区暗化，并为 10.5 的画质档与面板整理留下第一个可选画质开关与 BRP/读数入口；本段不主张时域累积与 TAA、不做光追 AO、不把 AO 乘到直射阴影与 SCGI 注入、不改 G-buffer 附件布局、不改透明层。

## 0. 立案期事实与口径

### 0.1 开工前基线

| 项 | 读数 | 口径与来源 |
|---|---|---|
| 立案起点提交 | `0198cbbd8d19f7423c2f4a280bbfdf233686be67` | 立案时 `git rev-parse HEAD`（`main`，10.1 收官之后） |
| 施工分支 | `goal/10.2`（从起点 HEAD 建，立案提交落在该分支） | 本计划对应的临时分支，起点 SHA 见登记报告 |
| 合成环境项现状 | 常量环境 = `albedo.rgb * frame.ambient_color.rgb`；IBL = `ibl.diffuse + ibl.specular`；SCGI 用 `mix(ibl.diffuse, replacement, coverage)` 替换漫反射；AO 只有 `matparam.g` 乘在直射上 | `ash_renderer/shaders/scene/gbuffer/gbuffer_composite.wgsl:188`、`:208-237` |
| AO 相关 BRP | 无：`ssao_set` / `ssao_stats` 都不存在 | `ash_renderer/src/remote/mod.rs` 方法与 `render_settings.rs` 方法族清单 |
| 帧计时点位 | 12 位，没有 SSAO 段 | `ash_renderer/src/render/stats/perf.rs:48-84` |
| 面板 GI 页 | 只有表面缓存开关/视图/形态/种子/整理，没有 AO 项 | `ash_renderer/src/overlay/tool_window/gi.rs:37-120` |
| 基线画面 | 未采（开工闸门 1 采：三机位关档图 + 9.10 锚点） | 本文 §5 |
| 基线性能 | 未采（开工闸门 1 采：Release 下 `frame_ms` 与各段 p50） | 本文 §5、§6 |

这些是开工前基线，不是本段施工结果。

### 0.2 现状核实清单

| 项 | 结论 | 依据 |
|---|---|---|
| 合成 AO 只作用直射 | `let occlusion = matparam.g;` 只喂直射累加；环境项完全不含 AO | `ash_renderer/shaders/scene/gbuffer/gbuffer_composite.wgsl:188`（环境项 `:233-237`） |
| G-buffer 采样集 b2 不是空位 | b2 = HDR 附件视图；整集 7 个 binding（0 albedo / 1 normal / 2 HDR / 3 depth / 4 sampler / 5 matparam / 6 emissive） | `ash_renderer/src/render/passes/gbuffer/gbuffer_targets.rs:50-75` |
| 采样集可见阶段含 compute | `FS_CS = FRAGMENT \| COMPUTE`；SSAO compute 可直接复用该集读深度与法线 | 同上 `:59-63` |
| 合成管线 6 集、无 push、无深度 | sets 依次 frame / gbuffer_sampled / cluster / shadow / IBL / scgi_sample；颜色格式 HDR；`push: None` | `ash_renderer/src/render/passes/lighting/composite_pipeline.rs:18-34` |
| 合成录制点与实参 | `FullscreenPass { view: hdr, sets: [frame, gbuffer_sampled, cluster, shadow, ibl, scgi] }`，在 G-buffer 转只读与 GI 之后、透明层之前 | `ash_renderer/src/render/frame/record.rs:294-311` |
| 「恒绑保布局」先例 | `scgi_set` 恒绑；`frame.scgi.x == 0` 时着色器不读，管线布局因此恒定 | `ash_renderer/src/render/frame/render_features.rs:56-58` |
| 尺寸重建链 | `rebuild_size_dependent(gpu, hiz, post, cluster, ray_tracing)` 逐项重建，失败 `or_warn`/`or_fatal` | `ash_renderer/src/driver/host/frame/resize_gate.rs:68-74` |
| 布局契约登记 | `ALL_PIPELINES` 与 `ALL_SETS` 全量清单，新增管线/集必须各加一条 | `ash_renderer/src/render/layout_registry.rs:66-158` |
| 存储图目标范式 | storage image：usage `STORAGE \| SAMPLED \| TRANSFER_SRC`、`initial_layout(UNDEFINED)`，建后一次性迁 `GENERAL` 并长住 | `ash_renderer/src/render/passes/gi/screen_gather.rs:760-800`、`:430-447` |
| 描述符写入器 | 写入按 `SetLayoutDesc` 校验种类与完备性，再一次性提交 | `ash_renderer/src/render/core/binding/descriptor_writer.rs:62-127` |
| compute 录制与交接屏障 | `record_grid(device, cb, sets, push, (gx, gy), Handoff)`；`Handoff::ComputeFragmentRead` 补 `COMPUTE\|FRAGMENT` 可见屏障 | `ash_renderer/src/render/core/record/compute.rs:96-146` |
| 图像回读 | `readback_image_region(ctx, image, aspect, layout, offset, extent, bytes_per_texel)` 结束恢复调用方布局；模块注释明确「不在帧循环内调用」 | `ash_renderer/src/render/core/device/readback.rs:51` |
| 面板读回取槽范式 | `targets.last_slot()` + `read_bins`，返回 `frame_age`/`stale` | `ash_renderer/src/remote/features/color_scope.rs:78-102` |
| 面板参数注入 | `ToolWindowParams` 持 `gi_settings: ResMut<GiSettings>` 并分发进 `GiPage` | `ash_renderer/src/overlay/tool_window/mod.rs:269, 341` |
| 帧计时消费点 | `fetch(slot) -> Result<Option<[f32; 9]>>`、`FrameSample` 定字段，加点位要同步 fetch/样本/消费点 | `ash_renderer/src/render/stats/perf.rs:342`、`:101-121` |
| 着色器自动发现 | `build.rs` 目录递归、stem 即入口名（跨目录禁重名），新增 `.wgsl` 自动进 `shader_abi.rs` | `ash_renderer/build.rs:40-110` |
| 帧槽数 | `MAX_FRAMES_IN_FLIGHT = 2` | `ash_renderer/src/render/core/device/context.rs:104` |

### 0.3 需要订正的上游口径

1. 主计划 §2.2 接口段写「合成 pass 的 group1 binding 2 为空位」——**不属实**：b2 是 HDR 附件（`gbuffer_targets.rs:52-75`）。本段因此新增**第 7 集**放下采样好的全分辨率 AO，不动 G-buffer 共享采样集，也不动它的布局生命周期。
2. 主计划任务 10.2.2 写「眼空间半球/地平线采样」——本段改为**世界空间半球采样**：帧 UBO 没有 view 矩阵（只有 `view_proj` 与 `inv_view_proj`），世界空间采样与眼空间等价且少一次逆变换；半径因此以**米**为单位（`radius_m`），主计划「视空间半径」的写法由本计划 §1 R3 取代。
3. 主计划任务 10.2.3 写「深度感知 5×5 双边」——本段先落 **2×2 象限双边**（按 0.3 ms 预算与半分辨率纹理的一倍纹素上采样推导）；5×5 列 §7 备选，触发条件写在那里。
4. 主计划任务 10.2.1 写 AO 目标 `R8_UNORM`——本段用 **`R32_SFLOAT`**：WGSL storage 纹理没有 `r8unorm` 格式，且 R32F 无量化，关档恒等与读数都干净。
5. 主计划 §2.2 边界「不改 G-buffer 布局」**保持成立**：本段只读 G-buffer 采样集，材质 AO 仍写 `matparam.g`。

## 1. 立案期裁决

### R1 材质 AO 不扩到环境项，SSAO 只乘环境项

- **裁决**：`matparam.g`（材质自带的 AO）保持现状，只乘直射阴影；SSAO 的 AO 只乘环境项（IBL 漫反射与常量环境）。
- **理由**：材质 AO 一旦改乘环境项，8 段以来所有 IBL/SCGI 标定读数全部漂移，9.10 的锚点偏差容差（≤2%）立刻失守；且两者语义不同（材质 AO 是贴图级遮蔽，SSAO 是屏幕空间接触遮蔽）。
- **施工落点**：只改 `gbuffer_composite.wgsl` 的环境项算式，`occlusion` 那一路不动。

### R2 AO 目标用 `R32_SFLOAT`、每帧槽一份、常驻 `GENERAL`

- **裁决**：半分辨率与全分辨率 AO 各按帧槽建一份（2 槽），格式 `R32_SFLOAT`，usage `STORAGE | SAMPLED | TRANSFER_SRC`，`initial_layout(UNDEFINED)`，建后一次性迁 `GENERAL` 并一直留着。
- **理由**：WGSL storage 纹理的合法格式里没有 8 位 unorm；R32F 无量化，`1.0` 乘上去是精确恒等，关档才能做到逐字节一致。常驻 `GENERAL` 沿用 SCGI 先例，省掉每帧进出 `TRANSFER_SRC` 的迁移。
- **施工落点**：`ssao_targets.rs` 的建图/迁移函数与 `SCGI` 同形；`R32_SFLOAT` 的 `STORAGE_IMAGE` 支持在设备上按 `get_physical_device_format_properties` 显式检查（现有 `ColorTarget::new_storage` 已有同类检查范式）。

### R3 世界空间半球采样，半径单位米

- **裁决**：以世界法线为轴取半球方向，采样半径 `radius_m`，命中判定用「期望视深 vs 实际视深」比较，bias 以米为单位。
- **理由**：帧 UBO 无 view 矩阵；`world_from_depth` 已存在于合成与 GI 上采样片段（`gbuffer_composite.wgsl:78-82`），深度比较只需 `view_proj` 的 `w` 行（透视下即正的视深），与调试图 MODE_DEPTH 的既有写法一致。
- **施工落点**：`shaders/scene/ambient/ssao.wgsl`；参数面 `radius_m` 缺省 0.5 m，`bias` 缺省 0.02 m。

### R4 上采样用 2×2 象限双边

- **裁决**：每个全分辨率像素取最近的 2×2 半分辨率纹素，按双线性权重 × 深度相似权重归一，输出全分辨率 AO。
- **理由**：半分辨率到全分辨率只差一倍纹素，2×2 正好覆盖；5×5 是 25 次权重计算，按 1600×900 估算会吃掉 0.3 ms 预算的大半。深度相似权重就能消掉几何边缘的 2×2 块感。
- **施工落点**：`shaders/scene/ambient/ssao_upsample.wgsl`。

### R5 关档绑 1×1 白 dummy，合成恒乘 AO

- **裁决**：`enabled == false` 时不录 SSAO/上采样 dispatch，合成照常绑第 7 集，但绑的是 1×1 白图（`R32_SFLOAT`，值 1.0）的 dummy 集；着色器无条件乘 AO。
- **理由**：管线布局恒定，关档不重建管线；乘 1.0 是 IEEE 精确恒等，逐字节回归成立。不要把开关做成 `if (enabled)` 分支去乘，那会多一条分支还仍要绑集。
- **施工落点**：`ssao_targets.rs` 建 dummy 图与 dummy 采样集（init 时一次）；`frame.rs` 按设置选槽集或 dummy。

### R6 合成新增第 7 集 `ssao_sample`

- **裁决**：新增 `SSAO_SAMPLE_SET_DESC`（b0 `SampledImage`，FRAGMENT，`textureLoad` 全分辨率 AO，不需要采样器），加进 `GBUFFER_COMPOSITE_PIPELINE.sets` 数组末尾；合成 shader 加 `@group(6) @binding(0)`。
- **理由**：G-buffer 共享集是 8 段共用布局，往里加绑定会让所有读它的管线都要重建；独立集只影响合成一条管线，也和 SCGI 单纹理集的先例一致。
- **施工落点**：`composite_pipeline.rs:18-34` 数组加一项、`layout_registry.rs` 的 `ALL_SETS` 加一条。

### R7 帧计时 12→16 位，末尾追加 `Ssao = 12`

- **裁决**：`QUERIES_PER_FRAME` 扩到 16；`TimestampPoint::Ssao = 12` 追加在枚举末尾（数值不复用、不重排）；`fetch` 返回数组扩到 10 维：`ssao = seg(SurfaceCache, Ssao)`、`lighting = seg(Ssao, Lighting)`，其余索引不变；`FrameSample` 加 `gpu_ssao_ms`。
- **理由**：9.4 与 9.5 已各追加过两位数点位，既有索引是既有文档与脚本的锚点；追加位是唯一不破坏既有消费点的做法。
- **施工落点**：`perf.rs` 模块注释表 +1 行、`frame.rs` 样本组装、BRP 与面板消费点同步。

### R8 BRP 方法族：`ssao_set` 写设置、`ssao_stats` 读画面

- **裁决**：`ash_renderer/ssao_set` 收 `enabled` / `radius` / `bias` / `intensity` / `steps`，缺省字段不动、逐字段校验、回写 `applied` 与 `current`；`ash_renderer/ssao_stats` 返回 `enabled`、当前参数、`ssao_ms` 分位、`mean_ao`、`dark_ratio`（AO < 0.8 的像素占比）与 `extent`，读回走 `readback_image_region`，取最近提交帧槽（`last_slot`）。
- **理由**：10.1 已确立「每段新增设置方法与读数方法」的 BRP 惯例；`color_scope` 的双方法（`_set` + `_get`）就是本段最贴近的模板。
- **施工落点**：`src/remote/features/ssao.rs`（新）或并入 `render_settings.rs`（施工时按文件体量定，倾向新文件）+ `remote/mod.rs` 注册两行。

### R9 面板在现有 GI 页追加一小节

- **裁决**：`GiPage` 里加一个 `ui.group`「环境光遮蔽（SSAO）」：开关、`radius_m`/`bias`/`intensity` 滑杆、`steps` 下拉、`ssao_ms` p50 一行读数，并注明 BRP 方法名与「关 = 不录 dispatch、AO 恒 1」。
- **理由**：主计划 §2.2 已定「先在现有 GI 页加一小节，10.5 再做页面级整理」；`ToolWindowParams` 已有 `perf` 账可算 p50，不需要新回读。
- **施工落点**：`overlay/tool_window/gi.rs` + `mod.rs` 的 `ToolWindowParams` 加 `ssao_settings: ResMut<SsaoSettings>`、传 p50 进页面；`init.rs` 资源表加 `SsaoSettings`。

## 2. 任务拆解

| 任务 | 交付 | 主要文件 | 出口判据 |
|---|---|---|---|
| 10.2.1 | AO 目标族、两条管线、初始化与尺寸重建接线 | `src/render/passes/ambient/{mod,ssao_targets,ssao_pass}.rs`、`init.rs`、`resize_gate.rs`、`layout_registry.rs` | 编译通过；`cargo run` 正常；BRP `layout_contract` 不报缺登记；关档画面与改前逐字节一致 |
| 10.2.2 | SSAO compute（半球采样 + 旋转核 + 厚度偏置） | `shaders/scene/ambient/ssao.wgsl` | 开档后 AO 图有接触区暗化、平地无 halo（读图 + `ssao_stats` 均值） |
| 10.2.3 | 深度感知上采样 | `shaders/scene/ambient/ssao_upsample.wgsl` | 半 vs 全分辨率 diff p99 ≤0.05（线性域）；无 2×2 块（读图） |
| 10.2.4 | 合成接线与关档恒等 | `gbuffer_composite.wgsl`、`composite_pipeline.rs`、`render_features.rs`、`record.rs`、`frame.rs` | 关档与立案基线逐字节一致；开档环境项变暗、SCGI 注入方向不变 |
| 10.2.5 | 帧计时 `ssao_ms`、BRP 方法族、面板小节 | `perf.rs`、`src/remote/features/ssao.rs`、`remote/mod.rs`、`overlay/tool_window/{mod,gi}.rs` | `ssao_set`/`ssao_stats` 可调可读；`ssao_ms` 出现在 perf 样本与面板 |
| 10.2.6 | 取证、判定与文档收官 | 本节 §6 产物、`10.2.K-*.md`、段 README、三处状态 | §4 判定线逐条有结果与证据；功能流程导读落盘 |

### 10.2.1 目标与管线骨架

1. 新模块 `src/render/passes/ambient/`：`mod.rs`（模块文档写清「G-buffer → SSAO 半分辨率 → 上采样 → 合成」数据流与关档语义）、`ssao_targets.rs`、`ssao_pass.rs`（录制束 `SsaoFrameDraw`）。
2. `ssao_targets.rs`：
   - 格式常量 `SSAO_FORMAT: vk::Format = vk::Format::R32_SFLOAT`；
   - `SSAO_STORAGE_SET_DESC`：b0 半分辨率 storage、b1 全分辨率 storage，`COMPUTE`；
   - `SSAO_SAMPLE_SET_DESC`：b0 全分辨率 sampled，`FRAGMENT`；
   - `SsaoTargets`：device、extent、每槽半/全图与视图、每槽 storage/sample 集、dummy 1×1 白图与 dummy sample 集、pool/layout、`last_slot`、`Drop`；
   - `new(ctx)`：建两份布局、dummy 图与 dummy 集（白图内容用一次 `cmd_clear_color_image` 或一次性 transfer 上传，施工时按最小实现选）；
   - `ensure(ctx, extent)`：尺寸不变 fast-path；变化时 `device_wait_idle` 后重建每槽图与集，并把图从 `UNDEFINED` 一次迁到 `GENERAL`；
   - `sample_set(slot)` / `dummy_sample_set()` / `storage_set(slot)` / `last_slot()`。
3. 管线：`SsaoPipelines { ssao: ComputePipeline, upsample: ComputePipeline }`，配方各一条（`SSAO_PIPELINE`、`SSAO_UPSAMPLE_PIPELINE`，入口 `cs_main`），`new(ctx, &layouts)`。
4. 接线：`init.rs` 在 `scgi_targets` 附近 `let mut ssao_targets = SsaoTargets::new(&ctx)?;` 并 `ensure(初始 extent)`；布局列表 push `ssao_targets.storage_layout()`/`sample_layout()`；管线创建紧随 `composite_pipeline`；两者进 `init_chain!`；`layout_registry.rs` 的 `ALL_PIPELINES`/`ALL_SETS` 各登记；`resize_gate.rs` 的 `rebuild_size_dependent` 加 `ssao_targets.ensure`。

### 10.2.2 SSAO 算法与 pass 录制

1. `ssao.wgsl`：`// #import frame`；`@group(0)` 帧 UBO、`@group(1)` G-buffer 采样集（b1 法线、b3 深度）、`@group(2) @binding(0)` 半分辨率 storage。
2. 每像素：半分辨率中心反投影得世界位置 `P` 与法线 `n`；切线基（Duff 等）+ 4×4 IGN 旋转；4 方向 × `steps` 步，步长按 `radius_m` 二次分布（近密远疏）；样本点 `P + dir * radius_m * t` 投到屏幕，`uv` 越界或深度为 0（天空）跳过；命中判定 `expected_w - actual_w > bias`；累计遮挡得到 `ao = saturate(1 - intensity * occ / steps)`。
3. 线性 dispatch（`if (gid.x >= half_w * half_h) return;`），`@compute @workgroup_size(64)`。
4. `ssao_pass.rs`：`SsaoFrameDraw` 持 `ComputeHandles`、`storage_set`、push 字节、dispatch 组数；`record(device, cb)` 依次绑 storage 集 dispatch，然后 `Handoff::ComputeReadWrite` 后接上采样（或把上采样单独成 draw，二者都用 `mark` 之外的屏障）；`ssao.wgsl` 与 `ssao_upsample.wgsl` 之间的屏障由 SSAO 侧 `ComputeReadWrite` 完成，上采样到合成的屏障由 `ComputeFragmentRead` 完成。

### 10.2.3 深度感知上采样

1. `ssao_upsample.wgsl`：`@group(0)` 帧 UBO、`@group(1)` G-buffer（b1 法线、b3 深度）、`@group(2)` b0 半分辨率 storage（读）、b1 全分辨率 storage（写）。
2. 每全分辨率像素：半分辨率坐标、2×2 邻居、双线性权重 × 深度相似权重（`1 / (1 + |Δ视深| / sigma)`），归一后加权；深度 sigma 走 push 常量（缺省 0.05 m）。
3. 线性 dispatch + 边界 return；`textureStore` 写全分辨率 AO。

### 10.2.4 合成接线与关档恒等

1. `render_features.rs`：加 `ssao: Option<SsaoFrameDraw>` 与恒绑 `ssao_set: DescSet`，字段注释写清「关档 = dummy 白图，乘 1.0 精确恒等」。
2. `record.rs`：`mark(TimestampPoint::SurfaceCache)` 之后、`FullscreenPass` 之前插 `if let Some(ssao) = feature.ssao { ssao.record(device, cb); }`，紧接着 `mark(TimestampPoint::Ssao)`；合成 `sets` 数组末尾加 `feature.ssao_set`。
3. `frame.rs`：`ssao.targets.ensure(&gpu.ctx, extent)`；`prepare_ssao(settings, targets)` 照 `prepare_gi` 的形状返回 `Option<SsaoFrameDraw>`（关档返回 `None`），并按设置选 `sample_set(slot)` 或 `dummy_sample_set()`。
4. `gbuffer_composite.wgsl`：加 `@group(6) @binding(0) var ssao_tex: texture_2d<f32>;`；`let ao = textureLoad(ssao_tex, clamp(px, 0, dims-1), 0).r;`；`ibl.diffuse` 先乘 `ao` 再参与 SCGI `mix`；常量环境 `albedo * ambient_color * ao`；`ibl.specular` 不乘；`occlusion` 那一路不动。
5. 关档回归：`ssao_set {"enabled": false}` 后与立案基线三机位同曝光逐字节比对。

### 10.2.5 帧计时、BRP 方法族与面板小节

1. `perf.rs`：`QUERIES_PER_FRAME = 16`；`TimestampPoint::Ssao = 12` 追加；模块注释表加一行「T12 ④.5 之后 SSAO+上采样 后 | SSAO（T5→T12，`fetch` 内单列）」；`fetch` 返回 `[f32; 10]` 并拆 `ssao`/`lighting`；`FrameSample` 加 `gpu_ssao_ms`。
2. 消费点：`frame.rs` 样本组装、BRP `perf_stats` 字段清单、面板帧统计页（若已有按字段列表展示则加一行）。
3. `ssao_set`/`ssao_stats`（新 feature 文件），`remote/mod.rs` 注册；`ssao_stats` 的 `mean_ao`/`dark_ratio` 由回读的 `R32F` 全分辨率图在 Rust 侧算。
4. `overlay/tool_window/gi.rs` 加小节：开关、三滑杆、`steps`、`ssao_ms` p50 一行、BRP 方法名一行；`mod.rs` 注参数；`init.rs` 资源表登记 `SsaoSettings`（缺省关）。

### 10.2.6 取证、判定与文档收官

1. 三机位（与 9.10 取证机位一致）：开/关各一张 LDR 截图，存 `.temp/10.2/`。
2. 平地/天空机位一张开档图 + `ssao_stats` 均值/暗部占比读数。
3. Sponza 柱础近景开档图 + 柱础/拱底区域的 `dark_ratio`。
4. 半分辨率 vs 全分辨率 diff（施工临时把上采样输出改成半分辨率放大对比，或在 shader 里加一次性 debug 分支后回退；读数记录 p99）。
5. `ssao_ms` p50、帧时间 p50 增量（Release，1600×900）。
6. 三机位 SCGI A/B 复跑 + 9.10 锚点 `l0_covered_mean` 偏差。
7. 逐任务写 `10.2.K-*.md`；段收官写无编号「功能流程导读」；更新段 README 判定线结果表、计划头部状态、路线图三处。

## 3. 边界：本段不主张什么

- 不做光追 AO、GTAO 多弹射、bent normal（归后续段/不立项）。
- 不做时域累积与 TAA；只做空间去噪（旋转核 + 深度感知上采样），画面上允许一定噪声，不以抖动/累积换质量。
- 不把 AO 乘到直射阴影：`occlusion = matparam.g` 那一路保持原样。
- 不把 AO 乘到 SCGI 注入项：`mix` 的 replacement 一侧不乘 AO，避免与 9.10 的缓存标定双计。
- 不改 G-buffer 附件布局与采样集：材质 AO 仍写 `matparam.g`，SSAO 只活在合成前的一张全分辨率贴图里。
- 不改透明层（Clustered Forward 不消费 AO）；透明物体上的 AO 缺失属已知局限，写进导读。
- 不改 BRP 既有方法名与参数语义；只增量 `ssao_set`/`ssao_stats`。
- 不做面板页面级整理（10.5），不新增工具窗口页。
- 不合并验收 10.1/10.3：本段分支 `goal/10.2` 独立判定；不触碰 9.11 的 SCGI P1 文件。

## 4. 判定线草案

每条先写成可复跑的断言，再施工；判定记录另立 `10.2.6-取证与判定.md`。

### 4.1 关档恒等

- **判定 1**：`ssao_set {"enabled": false}` 后，与立案基线（`0198cbbd8` 构建）同机位、同曝光、同后链参数的三张截图逐字节一致（`.temp/10.2/` 下 diff 为 0）。
- **判定 1b**：关档时 BRP `perf_stats` 不含 SSAO 段耗时或该段 ≈0；`ssao_stats` 报告 `enabled:false` 且不触发回读（`stale:true` 允许）。

### 4.2 质量

- **判定 2（无 halo）**：正对平地/天空的固定机位，`ssao_stats.mean_ao ≥ 0.98`，且该帧 AO 图最大值与 1.0 的差 ≤1e-3；读图看不到环绕物体的暗圈。
- **判定 3（接触区有效）**：Sponza 柱础/拱底同机位，`dark_ratio`（AO < 0.8 像素占比）≥5%，读图见连续阴影带而非孤立斑点。
- **判定 4（上采样无块感）**：半分辨率 AO 双线性放大 vs 本段全分辨率输出，线性域 diff p99 ≤0.05；读图无 2×2 网格块。
- **判定 5（不破坏既有画面）**：SCGI 开/关 A/B 的注入方向与 9.10 记录一致；复跑 9.10 锚点，`l0_covered_mean` 相对本段改前偏差 ≤2%。

### 4.3 性能与构建

- **判定 6a**：Release、1600×900、RTX 2060 上 `ssao_ms` p50 ≤0.3 ms（BRP `perf_stats` 或 `ssao_stats` 读数）。
- **判定 6b**：同一场景 `frame_ms` p50 相对关档增量 ≤0.5 ms。
- **判定 6c**：`cargo check`/`cargo build` 目标 crate 通过；`cargo clippy -p ash_renderer --all-targets` 零告警；不新增 crate 依赖。

### 4.4 契约不倒退

- **判定 7a**：运行期零 `VUID`/`VK-ERROR`（含 resize、开关切换、面板拖滑杆、BRP 连续调用）。
- **判定 7b**：BRP `ash_renderer/layout_contract` 通过（新管线/新集已登记，shader 入口与配方一一对账）。
- **判定 7c**：`cargo test -p ash_renderer` 既有单测不倒退（含 `scgi_consumer_enabled` 一类门控单测）。
- **判定 7d**：`frame_dump`、`perf_stats`、`ssao_stats` 三个读数入口都能取到数（不因新段断链）。

## 5. 前置闸门与施工顺序

- **闸门 0（立案后、开工前）**：本计划落盘 + 三处状态同步 + 段 README 任务面板建立；`git status` 干净；从 `0198cbbd8` 建 `goal/10.2`（已建则接续）。
- **闸门 1（改第一行代码前）**：重读本计划、步骤 10 README、主计划 §2.2 与 §3、9.10 审计报告与取证脚本；采改前基线（三机位关档截图、`frame_ms`/段 p50、9.10 锚点读数），落 `.temp/10.2/baseline/`。
- **每任务收口**：更新段 README 任务面板 → 代码检查（`cargo check` + clippy）→ 任务记录 `10.2.K-*.md` → docs/feat 分组提交（`goal/10.2`，不推送）。
- **顺序**：10.2.1 → 10.2.2 → 10.2.3 → 10.2.4 → 10.2.5 → 10.2.6。10.2.4 之前必须保持「关档不改变画面」，否则先停下排查布局/绑定问题。
- **段收官**：功能流程导读 + 判定线逐条结果 + 三处状态流转（步 README、路线图、计划头部）。

## 6. 取证命令与产物

只固定要采什么，不写死未核实的命令行；实际命令在记录里回填。

- `.temp/10.2/baseline/`：改前三机位关档截图、`perf_stats` JSON、9.10 锚点读数。
- `.temp/10.2/`：开/关成对截图（命名 `10.2-<机位>-<on|off>.png`）、平地机位、Sponza 近景、上采样对比图、`ssao_stats` JSON、`perf_stats` JSON、`layout_contract` JSON。
- 证据不入库（与 10.1 同约定）；docs 只引用路径与读数。
- 截图判定按《画面功能视觉验证》：`PrintWindow` 截取 + 会话模型读图，先采后判。

## 7. 风险、备选与回退

- **风险 A：半分辨率 + 2×2 双边在柱头雕花处发糊。** 备选 1：上采样改 3×3/5×5（仅在上采样 pass 内扩；性能预算不够时把 `steps` 降到 3 换预算）。备选 2：AO 做 3×3 中值后再上采样。
- **风险 B：halo（半径过大/偏置过小）。** 判据是判定 2；先调 `bias`（0.01–0.05 m），再调 `radius_m`（0.3–1.0 m），调参读数进记录，不写死默认值。
- **风险 C：近处过黑、远处失效。** 半分辨率下半径覆盖像素数随视距变化；世界空间半径本身已消掉大部分问题，若近处仍过黑则加屏幕半径钳制（`min(radius_m, 0.05 * 视深)`），施工时用固定机位标定。
- **风险 D：与 SCGI 双计。** 由 R1/边界保证（replacement 不乘 AO）；判定 5 直接复跑 9.10 锚点，偏差超 2% 就停下查算式。
- **风险 E：两帧在飞导致 AO 图 WAR 冲突。** 由 R2 每槽一份规避；若仍见闪烁，检查 `last_slot`/样本提交顺序，而不是加 `device_wait_idle`。
- **风险 F：`ssao_stats` 回读打断渲染。** 回读只走 BRP 调用（同 `color_scope`），不在帧循环；若读数抖动大，允许 `ssao_stats` 返回缓存样本（带 `frame_age`）。
- **风险 G：管线/布局生命周期。** 新增集与管线必须抄 SCGI 的建/拆顺序（`init_chain` + `Drop` + `device_wait_idle` 前置）；`layout_contract` 判定兜底。
- **回退**：任何一条判定线失守且修不动，先把 `enabled` 默认改回 `false` 并保留代码与读数，按「遗留」上报，不删代码、不改判定线。

## 8. 文件与锚点清单

**新增**

| 文件 | 作用 |
|---|---|
| `ash_renderer/src/render/passes/ambient/mod.rs` | 模块入口与数据流文档 |
| `ash_renderer/src/render/passes/ambient/ssao_targets.rs` | AO 图、两套布局、每槽集、dummy |
| `ash_renderer/src/render/passes/ambient/ssao_pass.rs` | SsaoFrameDraw 与两条 dispatch 的录制 |
| `ash_renderer/shaders/scene/ambient/ssao.wgsl` | 半球采样与 AO 写半分辨率 |
| `ash_renderer/shaders/scene/ambient/ssao_upsample.wgsl` | 2×2 象限深度感知上采样 |
| `ash_renderer/src/remote/features/ssao.rs` | `ssao_set` / `ssao_stats` |

**修改**

| 文件 | 改动 |
|---|---|
| `ash_renderer/src/render/passes/lighting/composite_pipeline.rs:18-34` | sets 末尾加 `SSAO_SAMPLE_SET_DESC` |
| `ash_renderer/shaders/scene/gbuffer/gbuffer_composite.wgsl` | 第 7 集绑定 + 环境项乘 AO |
| `ash_renderer/src/render/frame/render_features.rs` | `ssao` 束与 `ssao_set` |
| `ash_renderer/src/render/frame/record.rs:294-311` | SSAO 录制插入点与 `Ssao` 打点 |
| `ash_renderer/src/driver/host/frame.rs` | `prepare_ssao`、ensure、组装与选集 |
| `ash_renderer/src/driver/host/frame/resize_gate.rs:68-74` | `ssao_targets.ensure` |
| `ash_renderer/src/driver/host/init.rs` | 目标/管线创建、布局登记、资源表 |
| `ash_renderer/src/render/layout_registry.rs:66-158` | 新管线与两套集登记 |
| `ash_renderer/src/render/stats/perf.rs` | 16 位、`Ssao=12`、`fetch` 10 维、`gpu_ssao_ms` |
| `ash_renderer/src/remote/mod.rs` | 注册 `ssao_set`/`ssao_stats` |
| `ash_renderer/src/overlay/tool_window/mod.rs`、`gi.rs` | GI 页 SSAO 小节与参数注入 |

## 9. 与相邻段的关系

- **与 10.1（帧耗时先导）**：10.1 的 rings 四段已在 `FrameSample`；本段只追加 `gpu_ssao_ms` 一个字段与一个点位，不改 10.1 的四段语义。
- **与 10.3（Bloom 后链）**：本段插在合成前，10.3 在 Bloom 链上，二者不共享文件；唯一交叉点是 `record.rs` 的录制段插入点，按行区间隔离。
- **与 10.5（面板整理）**：本段只加一小节与两个 BRP 方法，页面级整理、画质档归 10.5。
- **与 9.7/9.10（SCGI）**：只读不写 SCGI 目标；AO 不进 `mix` 的 replacement 一侧；判定 5 复跑 9.10 锚点。
- **与 9.8/9.11**：不做 TAA/时域累积，不碰 SCGI P1；G-buffer 布局只读。
- **与步骤 8（G-buffer）**：只读采样集（深度/法线），不改附件与写入路径。
