# 施工计划：10.3 屏幕空间反射（SSR）

> 立案日期：2026-11-27（纯文档立案；零代码、未提交、未推送）
> 当前状态：✅ 收官（2026-11-27，修正轮后）：10.3.1–10.3.7 全部落盘并通过构建；判定线 10 过 / 3 部分 / 1 未测；逐条结论、修正轮定位与已知局限见[段 README](README.md) 与 [10.3.7](10.3.7-偏轴归零定位与步进修正.md)。
> 上游文档：[步骤 10 README](../README.md)、[主计划：表现与使用体验优化三段拆解](../施工计划：表现与使用体验优化三段拆解.md)、[学习目标实现步骤](../../学习目标实现步骤.md)、[全局光照谱系调研](../../../笔记/光照与材质/全局光照谱系调研.md)、[Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)、[9.8 局限自检](../../9-表面缓存全局光照/9.8-动态验收与压测/局限自检：SCGI已知局限对照.md)
> 文档分工：本文只写做什么、按什么顺序做、怎么验收；实际读数、截图与逐条判定另立 10.3.x 记录，不回写本文冒充已完成。

**一句话说明**：在合成写完 HDR 之后、透明层混合之前插一条屏幕空间反射支路（同帧拷贝 HDR → 半分辨率世界空间步进 → 全分辨率合成回写 HDR，用命中色替换该像素原本的 IBL 高光），为光滑金属、柱础与湿地面补上当帧场景内倒影，并把开关与读数做成 10.5 面板可直接挂的一节；本段只做单次命中的屏幕空间反射，不主张光追反射、平面反射、反射探针、多次弹射与时域累积，不改 G-buffer 附件布局、不改透明层行为、不改 SCGI 采样链。

## 0. 立案期事实与口径

### 0.1 开工前基线

| 项 | 读数 | 口径与来源 |
|---|---|---|
| 立案起点提交 | `c27b1ae7f32421912bfe3f01ed2d1348653157f1`（`main`，10.1/10.2 收官之后） | `git rev-parse HEAD`；本文件所属立案提交以它为父提交 |
| 施工分支 | `goal/10.3`（从起点 HEAD 建，立案提交落在该分支） | 沿用 `goal/10.1`、`goal/10.2` 的段分支约定（主计划 §3） |
| HDR 目标现状 | 每槽一张 `R16G16B16A16_SFLOAT`，用途 `COLOR_ATTACHMENT｜SAMPLED｜TRANSFER_SRC`，建目标时 `need_blend = true` | `src/render/core/target/color.rs:11`、`src/render/passes/gbuffer/gbuffer_targets.rs:268`（`:17` 头注解释为什么要混合：透明层在它上面混合） |
| 合成与后链顺序 | G-buffer → GI → SSAO（④.6）→ 合成写 HDR → 透明层 LOAD 混合 → `color_to_sampled(hdr)` → Bloom → tonemap | `src/render/frame/record.rs:297-300`、`:303-326`、`:328-360`、`:379`、`:402-410` |
| 帧计时现状 | `QUERIES_PER_FRAME = 13`（T0–T12），`fetch` 返回 10 维，`FrameSample` 最新追加项是 `gpu_ssao_ms` | `src/render/stats/perf.rs:45`、`:117`、`:348` |
| 契约读数 | `layout_contract`：52 条管线 / 30 个 set（10.2 收官读数） | `src/render/layout_registry.rs`；记录见 [10.2 段 README](../10.2-SSAO/README.md) 代码验证线 |
| 基线画面 | 未采（开工闸门 1 采：三机位关档截图 + `frame_dump` 的 HDR 域 + 9.10 锚点） | 判定 1/1b 的比对基准，采法与 10.2 相同 |
| 基线性能 | 未采（Release、1600×900 下 `frame_ms` 与各段 p50） | 判定 5 的增量基准 |

这些是开工前基线，不是本段施工结果。

### 0.2 现状核实清单

立案期逐条在仓库里核过的事实（都已按现役 checkout 核对，不是凭记忆）：

| 项 | 结论 | 依据 |
|---|---|---|
| HDR 是每槽一张、可采样、可作附件 | 属实 | `gbuffer_targets.rs:268` 的 `ColorTarget::new(ctx, extent, HDR_FORMAT, true, "HDR 中间目标")`，`ColorTarget::new` 用途含 `SAMPLED` |
| HDR 用途串不含 `STORAGE_IMAGE` | 属实：`ColorTarget::new` 给 `COLOR_ATTACHMENT｜SAMPLED｜TRANSFER_SRC`；`new_storage` 给 `STORAGE_IMAGE｜TRANSFER_SRC` 且不作附件 | `src/render/core/target/color.rs` 两条构造 |
| 步进所需的深度、法线、材质参数都在 G-buffer 采样集里 | 属实：b1 normal、b3 depth、b4 matparam、b4/采样器在 b5 | `gbuffer_targets.rs` 的 `GBUFFER_SAMPLED_SET_DESC` |
| compute 能读 G-buffer 采样集 | 属实：该集 stage 为 `FS_CS`，SSAO 的 compute 已在这样用 | `gbuffer_targets.rs`、`src/render/passes/ambient/ssao_pass.rs` |
| frame UBO 没有视图矩阵，只有 `view_proj`/`inv_view_proj`/`camera_pos` 等 | 属实 | `shaders/common/frame.wgsl:5-37` |
| 合成 shader 里 IBL 高光只在 `ibl_environmental` 一处算，且该函数是**本文局部函数** | 属实：全仓 grep 只有定义与一处调用 | `shaders/scene/gbuffer/gbuffer_composite.wgsl:104-142` |
| 环境项在高光上乘 `EXPOSURE` 后才进 HDR | 属实：`const EXPOSURE: f32 = 0.00016;`，合成末行 `(light_sum + environment) * EXPOSURE + emissive` | `shaders/common/brdf.wgsl:15`、`gbuffer_composite.wgsl:262-266` |
| 因此 SSR 无法“在 IBL 之前”算，只能“先扣再加” | 结论：反射合成必须重算一次该像素的 IBL 高光并减去 | 由上面两条推出；§1 R5 是落地方案 |
| 全屏管线不支持混合、也没有 push 常量 | 属实：`pipeline/fullscreen.rs:1-4` 头注「不混合、单颜色附件」、`:22`「全屏管线没有 push」；`FullscreenPass` 结构无 push 字段 | `src/render/core/pipeline/fullscreen.rs`、`src/render/core/record/fullscreen.rs` |
| 反射图不能直接由现有的两条目标构造承担（要 compute 写 + 片元读） | 属实 | 见本表第 2 行；本段需新构造或加用途（R6） |
| 定时器是 13 个查询、10 维 `fetch`，新点位数值追加在枚举末尾 | 属实；先例 `TimestampPoint::Ssao = 12` 数值在末尾而录制位在 ④.6，证明“数值追加 + 录制插中段”可行 | `src/render/stats/perf.rs:45`、`:117`、`:348` |
| `stopwatch.rs` 的 `gpu_sections` 长度 10，映射 `[8]=ibl_capture`、`[9]=ssao`、`[7]=total` | 属实，加 SSR 要同步长度、映射与注释 | `src/driver/host/frame/stopwatch.rs:31`、`:54`、`:70`、`:134-167` |
| 面板分位缓存只跟踪 CPU/total/lighting 三个字段 | 属实：`PerfPanelCache { cpu, gpu_total, gpu_lighting }` | `src/overlay/tool_window/frame_stats.rs:62-91` |
| BRP 注册要改三处、能力文件要进 `features/mod.rs` | 属实：文档表、`use` 清单、`with_method_main` 三处 | `src/remote/mod.rs:26`、`:96`、`:182-184`、`src/remote/features/mod.rs` |
| 关档不回读是既有口径 | 属实：`ssao.rs` 头注写明“读数不替设置方开 GPU 段” | `src/remote/features/ssao.rs` 头注 |
| 尺寸相关资源走“布局/池进进程级、`ensure` 只换 image/view 并重写描述符集” | 属实 | `src/render/passes/ambient/ssao_targets.rs:1-20` 头注（2026-11-24 布局悬空教训） |
| 尺寸变化统一在 `resize_gate.rs` 里重建 | 属实：`rebuild_size_dependent(..., ssao_targets)` | `src/driver/host/frame/resize_gate.rs:68-113` |
| 透明层自算环境项，不调 `ibl_environmental` | 属实：`transparent_forward.wgsl:99` 附近有自己的环境项 | `shaders/scene/transparent/transparent_forward.wgsl` |
| `shaders/scene/` 下没有 `reflection/` 目录，`passes/` 下没有 SSR 模块 | 属实：现有子目录为 ambient/gbuffer/gi/ibl/ray_tracing/shadow/transparent | `ash_renderer/shaders/scene/`、`ash_renderer/src/render/passes/` |
| 新增 shader 入口无需手工登记 | 属实：`shaders/<分类>/` 递归枚举、产物汇成 `shader_abi` | `ash_renderer/build.rs` 头注 |
| `mode` 调试图是合成 shader 内的逐像素表达式（1–12），SSR 是后置 pass | 属实 | `frame_uniform_table.rs` 的 `FRAME_MODE_*`、`gbuffer_composite.wgsl` 头注 |
| 反射合成是全分辨率片元（`ibl_roughness` 依赖 `dpdx/dpdy`） | 结论：反射的合成必须全分辨率，否则 specular AA 口径与合成分支不一致 | `shaders/common/ibl.wgsl` 的 `ibl_roughness` |

### 0.3 订正上游口径

- 主计划 §2.3 写“眼空间 ray march”——本段改为**世界空间步进**：frame UBO 没有视图矩阵（见 §0.2），补矩阵要动 352 B 契约与 `offset_of!` 断言三处同步（`frame_uniform_table.rs`），得不偿失；用 `view_proj` 把世界点投影取 uv 与视深，与 `world_from_depth`（SSAO 已在用）是同一套口径。
- 主计划 §2.3 的“在合成后 HDR 上叠反射”——本段明确是“**先扣该像素的 IBL 高光、再加命中色**”，否则光滑金属会同时拿到 IBL 环境反射与 SSR 命中色两份高光，画面比改前更亮（9.8 局限自检第 3 条记录的现状正是“只有 IBL 环境反射”）。
- 主计划判定线第 3 条“粗糙度门控”——本段落成可测形式：`ssr_stats` 在粗糙区窗口里的 `hit_ratio` 为 0（见 §4 判定 3）。
- 名词口径：本段统一称「反射图」= 半分辨率 RGBA16F，rgb 为命中色、a 为命中置信度 `w`；不引入“反射缓冲/反射缓存”等第二套说法。

## 1. 立案期裁决

### R1 反射合成的位置：新 pass，插在合成与透明层之间

- **裁决**：不改合成分支（`gbuffer_composite.wgsl` 只做 R5 的函数抽取，不加 SSR 逻辑）；在 `record.rs` 的合成段之后、透明层段之前插入「HDR 拷贝 → SSR 步进 → SSR 合成」三段。
- **理由**：透明层以 LOAD 在 HDR 上混合；若 SSR 放到透明层之后，透明像素也会被反射改写且没有 mask 可区分（主计划边界“透明层不接 SSR”）。插在合成之后同时满足主计划 §4 行 316「10.3 在合成之后」与行 317「反射合成在 Bloom 之前」。
- **施工落点**：`record.rs` 合成 `FullscreenPass` 之后依次录三段；第一段前把 HDR 从 `COLOR_ATTACHMENT_OPTIMAL` 显式转到可采样读，第三段以附件形式写回 HDR，并用 `ImageBarrier::transition` + `MemoryBarrierDesc` 与后面透明层的附件写建立执行依赖（透明层当前已有一次同型 barrier，照抄写法）。

### R2 同帧拷贝（主计划 §5 D3 落定）

- **裁决**：每帧先用一条全屏 pass 把 HDR 拷到 `hdr_copy`，SSR 的步进与合成都只读副本，写只写 HDR 一个附件。
- **理由**：主计划 §5 D3 已建议同帧拷贝（无时域滞后，代价 +1 次全屏 copy）；SSR 合成要读命中色又要写回 HDR，同图既读又写需要格式用途与布局两边都对，先例（SSAO 两 pass、Bloom）都是“读 A 写 B”。
- **施工落点**：`reflection_copy_pipeline.rs` + `shaders/scene/reflection/reflection_copy.wgsl`，pipeline sets 只用 `[GBUFFER_SAMPLED_SET_DESC]`（b2 是 HDR、b5 是采样器），输出 `hdr_copy`；`clear: None`（每像素写满）。

### R3 步进空间：世界空间（订正主计划措辞）

- **裁决**：射线在世界空间推进 `P(t) = origin + dir · t`，每步用 `frame.view_proj` 投影取 uv，并用 `clip.w` 作为该点的视深与 G-buffer 深度换算出的视深比较。
- **理由**：见 §0.3 第一条；`frame.view_proj` 第 4 行点积即 `clip.w`（`clip_w()` 的既有写法在 `ssao.wgsl`），reverse-Z 与天空 `depth <= 1e-6` 早退也照抄。
- **施工落点**：`ssr.wgsl` 从 `shaders/scene/ambient/ssao.wgsl` 抄 `world_from_depth`、`clip_w`、天空早退三个片段，不新造第二套反投影口径。

### R4 分辨率：步进半分辨率、合成全分辨率

- **裁决**：步进 compute 按 `half_extent` 派发并写半分辨率反射图；反射合成是全分辨率全屏 pass。
- **理由**：成本主要在步进（步数 × 像素）；而 `ibl_roughness` 需要 `dpdx/dpdy`，在源文件里的调用位置决定导数，必须在全分辨率片元里算才能与合成分支同口径。
- **施工落点**：`ssr.wgsl` 的 `cs_main`；`reflection_composite.wgsl` 的 `fs_main` 用 `gbuf_sampler` 双线性采样半分辨率反射图。

### R5 IBL 高光扣除：抽公共函数，两处共用

- **裁决**：把 `ibl_environmental` 的高光分支抽到 `shaders/common/ibl.wgsl`，新增 `fn ibl_specular_from(env, samp, dfg, n, v, no_v, p_ibl, f0, intensity) -> vec3f`；`ibl_environmental` 改为调它（行为不变），SSR 合成也调它，然后 `HDR' = hdr_copy + w · (L_hit − ibl_spec · EXPOSURE)`。
- **理由**：`ibl_environmental` 是合成 shader 的局部函数，照抄一份必然漂移（`ibl_normal` 翻转、`ibl_roughness` 的导数位置、`frame.ibl_intensity` 的乘点三处都可能写错一处）；`common/ibl.wgsl` 已经是“不声明绑定、纹理/采样器作函数参数”的公共片段范式（`ibl_specular` 就是），抽取是顺势。`w = 0` 时加 0，保证关档与不命中像素精确恒等；`frame.ibl_enabled == 0` 时 `ibl_spec` 为 0，减项自然消失。
- **施工落点**：`ibl.wgsl` 加函数；`gbuffer_composite.wgsl:104-142` 的 `ibl_environmental` 改为调它（**保持调用在源文件的同一位置、同一语句顺序**，避免导数依赖变化）；`reflection_composite.wgsl` `#import ibl` 后同样调用，set 上加 `IBL_SET_DESC`。

### R6 反射图形态：半分辨率 RGBA16F，a 通道装置信度

- **裁决**：反射图一张半分辨率 `R16G16B16A16_SFLOAT`，rgb = 命中色（已乘过的 HDR 值），a = 命中置信度 `w`（已折进强度、边缘 fade、粗糙度门控）；用途必须同时满足 `STORAGE_IMAGE`（compute 写）、`SAMPLED`（片元读）、`TRANSFER_SRC`（读数回读）。
- **理由**：融合权重必须跟着命中色走，否则合成端无法区分“没命中”与“命中了黑物体”；同格式省一次转换；`hit_ratio` 直接统计 a 通道，不必再加一张掩码图。
- **施工落点**：`reflection_targets.rs` 里为该用途新构造图像（`ColorTarget` 现有两条构造都不给“storage + sampled”，这是本段唯一要碰的“核心小改动”，只加构造不改既有语义）；每槽两张：全分辨 `hdr_copy`、半分辨反射图。

### R7 关档语义：三段全跳过，不需要 dummy

- **裁决**：`ReflectionSettings.enabled = false` 时 `prepare_reflection(...)` 返回 `None`，三段零命令零屏障；HDR 直接进透明层。合成 pass 不绑任何反射集（不像 SSAO 那样需要 1×1 dummy），因为整个 pass 不录。
- **理由**：10.1/10.2 已把“关 = 基线逐字节”做成段级硬约定（主计划 §4 行 314）；SSAO 需要 dummy 是因为它挂在合成 shader 的固定 layout 里，SSR 是独立 pass，能更彻底。
- **施工落点**：`RenderFeatures` 加 `reflection: Option<ReflectionFrameDraw>`；`record.rs` 三段各自 `if let Some(...)`。

### R8 计时点位：末尾追加 `Composite = 13`、`Ssr = 14`

- **裁决**：`QUERIES_PER_FRAME` 13 → 15；`TimestampPoint` 末尾追加 `Composite = 13`（合成 pass 之后）与 `Ssr = 14`（SSR 合成 pass 之后）；`fetch` 返回数组 10 → 11 维，新元素在末尾；`FrameSample` 加 `gpu_ssr_ms`；`stopwatch.rs` 的 `gpu_sections` 10 → 11，`[10] = ssr`。
- **理由**：`ssr_ms` 要对 1.5 ms 预算判定，必须把 SSR 从既有 `Lighting` 段里分出来；而“合成 → 透明层”的这一段差值只有单列一个“合成结束”点才能切开。数值追加、录制位插中段有 `Ssao = 12` 先例（主计划 §5 D5 口径：追加不重排）。
- **施工落点**：`perf.rs`（枚举、模块注释表 T13/T14 两行、`QUERIES_PER_FRAME`、`fetch` 维数与新 `seg`、`FrameSample`）、`stopwatch.rs`、`src/remote/census/perf.rs` 的字段块与逐样本块、`features/ssr.rs` 的 p50。

### R9 调试图与 `RenderMode`：非 Lambert 时整段跳过

- **裁决**：宿主读 `Res<RenderMode>`，只要不是 `Lambert` 就 `prepare_reflection` 返 `None`。
- **理由**：`mode 1..12` 是逐像素表达式视图（法线/粗糙度/IBL 高光等），SSR 是后置 pass，会把反射叠到调试图上，破坏“调试图等于该表达式”的既有用法与 10.2 判定 4 的旁证口径。`ssr_stats` 里带 `render_mode`，便于排查“开了没效果”。
- **施工落点**：`driver/host/frame/reflection.rs` 里取 `Res<RenderMode>`；BRP `ssr_set.enabled` 不因模式改写设置值。

### R10 面板落点：IBL 页加一节

- **裁决**：在 `overlay/tool_window/ibl.rs` 追加「屏幕空间反射（SSR，10.3）」小节（开关 + 距离/厚度/步数/边缘 fade/粗糙度门控/强度滑杆 + 一行 `ssr_ms` 与 BRP 提示），`ToolWindowParams` 加 `reflection_settings` 字段并在页分发处传入。
- **理由**：主计划允许“天空/IBL 页或管线页”；反射本质是 IBL 高光的替代品，放 IBL 页语义最近；10.2 在 Gi 页加 SSAO 小节是先例。IA 整理与持久化归 10.5（主计划 §4）。
- **施工落点**：`ibl.rs`、`overlay/tool_window/mod.rs`（参数与分发）、`src/driver/host/init.rs` 资源表登记 `ReflectionSettings`。

### R11 BRP 命名（主计划 §5 D7 落定）

- **裁决**：`ash_renderer/ssr_set` 与 `ash_renderer/ssr_stats`，首版不提供反射图 dump 方法。
- **理由**：D7 已定名；`ssao_dump` 是“回读一张 AO 图”的特例，而反射图的 a 通道是权重、rgb 是“命中色”而非最终画面，单看容易误判，读数用 `hit_ratio` + p50 足够，要看画面走 `frame_dump` 的 HDR 域。
- **施工落点**：`src/remote/features/ssr.rs`（两个方法）+ `remote/mod.rs` 三处注册；`ssr_set` 空参、越界、类型错误都走既有 `invalid_params` 文案风格；先全校验再落值。

### R12 参数通路：march 走 compute push，合成端不需要参数集

- **裁决**：步进的步数、厚度、最大距离、强度、边缘 fade、粗糙度门控、全/半尺寸走 48 B compute push 常量；反射合成本身**不引入参数 UBO**——强度与门控已在 march 里折进 `w`，合成只需 `hdr_copy` + 反射图 + G-buffer + IBL。
- **理由**：全屏管线没有 push（`pipeline/fullscreen.rs:22`），为它加 push 要改核心 `PipelineDesc` 语义与 `FullscreenPass` 结构（本段不碰）；而把强度折进 `w` 之后，合成端确实只差“逐像素重算 IBL 高光”，那是纯数据依赖，不需要新参数。
- **施工落点**：`ssr.wgsl` 的 `struct PushSsr { full_size: vec2u, half_size: vec2u, steps: u32, max_t: f32, thickness: f32, edge_fade: f32, roughness_cutoff: f32, intensity: f32 }`（48 B，字段按 4 字节对齐排）；宿主侧手写 `to_ne_bytes` 布局（照 `ssao_push` 的写法）。

### R13 实施顺序上的取巧：先“只加不扣”，再“抽取并扣除”

- **裁决**：10.3.4 分两小步——先只做 `HDR = hdr_copy + w · L_hit`（不扣 IBL），确认步进与合成接线正确；再接 R5 的抽取与减项。
- **理由**：一步到底时，若画面亮了/暗了，分不清是步进命中错了、坐标错了还是减项口径错了；先加后扣能把变量分开，也让判定 1b（抽取前后 IBL 高光恒等）在抽取那一步单独成立。
- **施工落点**：`10.3.4` 记录里两小步各留一组开/关截图。

## 2. 任务拆解

| 任务 | 交付 | 主要文件 | 出口判据 |
|---|---|---|---|
| 10.3.1 | 反射目标族（全分辨 `hdr_copy` + 半分辨反射图，每槽一份）、`ReflectionSettings`、初始化与尺寸重建接线 | `src/render/passes/reflection/{mod.rs,reflection_targets.rs}`、`src/render/core/target/color.rs`、`src/driver/host/init.rs`、`src/driver/host/frame/resize_gate.rs`、`src/render/mod.rs` | 编译通过；`cargo run` 正常；resize 后尺寸正确（读数或 log 可查）；关档画面与改前逐字节一致 |
| 10.3.2 | HDR 拷贝通道（全屏 pass 写 `hdr_copy`）与录制插入点 | `src/render/passes/reflection/reflection_copy_pipeline.rs`、`shaders/scene/reflection/reflection_copy.wgsl`、`src/render/frame/record.rs`、`src/render/frame/render_features.rs` | 关档逐字节恒等；开档时 `hdr_copy` 与 HDR 一致（`frame_dump` 比对）；零 VUID |
| 10.3.3 | 步进核心（半分辨率 compute，线性步进 + 厚度 + 边缘 fade + 粗糙度门控，参数走 push） | `src/render/passes/reflection/ssr_pass.rs`、`shaders/scene/reflection/ssr.wgsl`、`src/driver/host/frame/reflection.rs`、`src/driver/host/frame.rs` | 反射图能看出倒影（读数或临时直出）；`ssr_ms` 有非零读数；关档恒等不受影响 |
| 10.3.4 | 反射合成（全分辨率 + R5 抽取 + R13 两小步） | `src/render/passes/reflection/reflection_composite_pipeline.rs`、`shaders/scene/reflection/reflection_composite.wgsl`、`shaders/common/ibl.wgsl`、`shaders/scene/gbuffer/gbuffer_composite.wgsl`、`src/render/frame/record.rs` | 开档出现倒影；关档与“只加不扣”那步的中间态可复现；抽取前后 IBL 高光视图逐字节一致（判定 1b） |
| 10.3.5 | 帧计时（R8）、BRP 方法族（R11）、IBL 页小节（R10）、布局登记 | `src/render/stats/perf.rs`、`src/driver/host/frame/stopwatch.rs`、`src/remote/features/ssr.rs`、`src/remote/mod.rs`、`src/overlay/tool_window/{ibl.rs,mod.rs}`、`src/render/layout_registry.rs` | `perf_stats` 有 `gpu_ssr_ms`；`ssr_set`/`ssr_stats` 可用；面板开关生效；`layout_contract` 通过 |
| 10.3.6 | 标定场景、判定线与取证、段收官 | `.temp/10.3/`（证据不入库）、`10.3.6-取证与判定.md`、`10.3-SSR/README.md`、9.8 局限自检口径订正 | 7 组判定逐条有结论（通过/部分/不通过）；README 收官；状态三处流转 |

### 10.3.1 反射目标族、设置资源与尺寸重建接线

1. `src/render/passes/reflection/` 新建模块：`mod.rs` 定义 `#[derive(Resource)] pub struct ReflectionSettings { enabled: bool, max_distance_m: f32, thickness_m: f32, steps: u32, edge_fade: f32, roughness_cutoff: f32, intensity: f32 }`，`Default` 里 `enabled: false`（关 = 基线），其余给立案建议初值，开工闸门 1 用标定场景再定。
2. `reflection_targets.rs`：`pub const REFLECTION_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;`（与 HDR 同格式）；`pub fn half_extent(extent)`（复用 `ssao_targets.rs:46` 的写法）；`ReflectionTargets::new(ctx)` 只建布局与描述符池（进程级），`ensure(ctx, extent)` 只换 image/view 并重写同一批描述符集（照 `ssao_targets.rs:1-20` 头注的四级分类：布局不可随尺寸重建）。
3. 每槽两张：全分辨 `hdr_copy`（`ColorTarget::new(ctx, extent, HDR_FORMAT, false, "…")`，不需要混合）+ 半分辨反射图（见下一步的用途）。
4. 反射图的用途：在 `color.rs` 增加一条新构造（或给现有构造加用途参数），旗标 = `STORAGE_IMAGE｜SAMPLED｜TRANSFER_SRC`；`initialLayout = UNDEFINED` 建好后迁到 `GENERAL` 常住（SSAO 两张 AO 图同法，省每帧迁移）。
5. 描述符集：`REFLECTION_STORAGE_SET_DESC`（b0 `hdr_copy` SampledImage/COMPUTE、b1 反射图 StorageImage/COMPUTE）、`REFLECTION_SAMPLE_SET_DESC`（b0 `hdr_copy` SampledImage/FRAGMENT、b1 反射图 SampledImage/FRAGMENT）；两集的采样器用 G-buffer 集里的 `gbuf_sampler`（跨集取采样器合法，`ibl.wgsl` 就是“采样器作参数”的写法），不再建第二个采样器。
6. `init.rs`：`ReflectionTargets::new` + `ensure(swapchain.extent)`，两条布局登记进 `SetLayouts`，`ReflectionSettings::default()` 进资源表；`src/render/mod.rs` re-export。
7. `resize_gate.rs`：`rebuild_size_dependent(...)` 加一个参数与一段 `reflection_targets.ensure(ctx, extent).or_fatal("反射目标重建失败，渲染链无法继续")`。
8. **出口**：`cargo check`/`clippy` 净；`cargo run` 起得来；resize 窗口后无 VUID；关档画面与改前逐字节一致（此时还没有任何 pass，只验证目标与布局不破坏既有链）。

### 10.3.2 HDR 拷贝通道

1. `shaders/scene/reflection/reflection_copy.wgsl`：`// #import fullscreen`，`vs_main` 全屏三角，`fs_main` 采样 set1 b2（HDR）后原样返回（`textureSampleLevel(..., 0.0)`，不引入额外色彩运算）。
2. `reflection_copy_pipeline.rs`：`FullscreenPipeline`，`color_formats: &[HDR_FORMAT]`，`sets: &[&GBUFFER_SAMPLED_SET_DESC]`，`push: None`。
3. `record.rs`：合成 `FullscreenPass` 之后插入 `FullscreenPass { view: hdr_copy_view, clear: None, pipeline: reflection_copy, sets: [gbuffer.sampled_set(slot)] }`；HDR 在这之前要能被采样读（合成写完 → `transition` 到 `SHADER_READ_ONLY`），`hdr_copy` 写字前用 `discard_to_color`。
4. 关档：拷贝也随之跳过（它唯一的消费者是 SSR）。
5. **出口**：开档时把 `hdr_copy` 回读或 `frame_dump` 与 HDR 比对一致；关档逐字节恒等；零 VUID。

### 10.3.3 步进核心

1. `ssr.wgsl`：`cs_main` 按 `half_size` 派发（`div_ceil(8).max(1)`，照 `groups_of`）；从 b0 取全分辨率 HDR 副本、b1 写反射图；从 G-buffer 集读 depth（b3）、normal（b1）、matparam（b5）与采样器（b4）。
2. 重建与合成同口径的像素参数：`getStandardPixelParams(albedo.rgb, matparam.r, matparam.b, matparam.a)`（`PixelParams` 在 `common/brdf.wgsl:70-99`），`V = normalize(frame.camera_pos.xyz - world_pos)`，`R = reflect(-V, n)`，`n` 用 `ibl_normal(n, V)`（与合成里翻转口径一致）。
3. 线性步进：`t` 从近到远均匀分布 `steps` 段，每步投影取 uv 与 `t_scene = clip.w`，命中条件 `|t_scene - t_ray| < thickness_m` 且 uv 在 `[0,1]`；命中后做 4 次二分细化，取命中点的 `hdr_copy` 颜色作为 `L_hit`（已含 `EXPOSURE`）。
4. 权重 `w = gate_roughness · gate_distance · edge_fade`，其中 `gate_roughness` 由 `matparam.b > roughness_cutoff` 直接给 0（粗糙面不参与），`edge_fade` 按 uv 距边界的距离在 start fade 区间内线性降到 0，最后整体乘 `intensity` 并 clamp 到 `[0,1]`，写进 a 通道。
5. push 常量按 R12 的 48 B 布局手写（宿主侧 `to_ne_bytes`），字段按 4 字节对齐。
6. 天空像素（`depth <= 1e-6`）与背面直接写 0；`frame.ibl_enabled == 0` 时仍然写命中色（减项由合成端按 0 处理），但 `w` 不变——保证“IBL 关掉也能看 SSR”是可自证的。
7. `ssr_pass.rs`：`static SSR_PIPELINE: PipelineDesc { label: "ssr", shader: &shader_abi::SSR, entries: &[c"cs_main"], sets: &[&FRAME_UNIFORM_SET_DESC, &GBUFFER_SAMPLED_SET_DESC, &REFLECTION_STORAGE_SET_DESC], push: Some(PushRange { stages: COMPUTE, size: 48 }), color_formats: &[], depth_format: None }`；`#[derive(Resource)] pub struct ReflectionPipelines { march, composite }`（composite 到 10.3.4 再用）；`ReflectionFrameDraw { march: ComputeHandles, storage_set, frame_set, gbuffer_set, half_size, push }`，`record()` 用 `record_grid(..., Handoff::ComputeReadWrite)`。
8. **出口**：反射图里有结构清晰的倒影（临时直出或回读）；`ssr_ms` 读数非零；关档恒等未破。

### 10.3.4 反射合成（含 R5 抽取与 R13 两小步）

1. 第一小步（只加不扣）：`reflection_composite.wgsl` 的 `fs_main` 采样反射图，`out = hdr_copy + clamp(w, 0, 1) * L_hit`；`reflection_composite_pipeline.rs` 用 `FullscreenPipeline`，`color_formats: &[HDR_FORMAT]`，`sets: &[&FRAME_UNIFORM_SET_DESC, &GBUFFER_SAMPLED_SET_DESC, &REFLECTION_SAMPLE_SET_DESC]`；`record.rs` 第三段写到 HDR 附件；开档截图确认倒影位置正确。
2. 第二小步（抽取并扣除）：`ibl.wgsl` 加 `ibl_specular_from(...)`（R5 签名），`gbuffer_composite.wgsl` 的 `ibl_environmental` 改为调它且**调用位置不变**；`reflection_composite.wgsl` `#import ibl` 后同样调它算 `ibl_spec`，`out = hdr_copy + clamp(w,0,1) * (L_hit - ibl_spec * EXPOSURE)`；集上加 `IBL_SET_DESC` 为第 4 个 set。
3. 合成端还要把 G-buffer 的 albedo/matparam 接到同一组绑定（b0/b5），保证 `PixelParams` 与 `V` 与合成 pass 严格同式。
4. 关档：三段全跳（R7）；开档但未命中（`w = 0`）时 `out` 精确等于 `hdr_copy`，即“关档与不命中”两条路径都恒等。
5. **出口**：判定 1（关档）与判定 1b（抽取前后 IBL 高光视图逐字节一致）成立；倒影出现且金属不再“IBL + SSR 两份高光”；`MODE_IBL_SPECULAR` 视图（`mode 10`）在抽取前后一致。

### 10.3.5 帧计时、BRP 方法族与面板小节

1. `perf.rs`：`QUERIES_PER_FRAME = 15`；`TimestampPoint` 末尾追加 `Composite = 13`、`Ssr = 14`；模块注释表加 T13/T14 两行（说明“合成结束”这一列是为了把 SSR 从 Lighting 段里切出来）；`fetch` 返回 `[f32; 11]`，新元素 `seg(Composite, Ssr)` 追加末尾；`FrameSample` 加 `gpu_ssr_ms`。
2. `record.rs`：合成 pass 之后 `mark(Composite)`，SSR 合成之后 `mark(Ssr)`。
3. `stopwatch.rs`：`gpu_sections` 10 → 11（`start`/`wait_slot_done` 的 `[0.0; N]` 同步），`push` 加 `gpu_ssr_ms = gpu_sections[10]`，注释表补行。
4. 消费点：`src/remote/census/perf.rs` 的 `percentile_block` 与逐样本块；`features/ssr.rs` 的 `.percentile(0.5, |s| s.gpu_ssr_ms)`。
5. `src/remote/features/ssr.rs`：`SetParams { enabled, max_distance_m, thickness_m, steps, edge_fade, roughness_cutoff, intensity }` 全 `Option<_>`，先全校验再落值，空参 `invalid_params`；`StatsParams { slot: Option<u32>, rect: Option<[u32;4]> }`（rect 是“粗糙区窗口 / 镜面窗口”的读数窗口，越界裁到画面内，裁空返 null，不回退整帧）；`ssr_stats` 读 `Res<ReflectionSettings>`、`Res<ReflectionTargets>`、`Res<PerfLedger>`、`Res<RenderMode>`，关档（或 extent 0）直接返 `enabled: false` 且**不回读**（照 `ssao.rs` 头注的口径：读数不替设置方开 GPU 段）；回读 a 通道用 `readback_image_region`（抽样，`READBACK_STRIDE` 风格，`readback.rs:51`）。
6. `remote/mod.rs` 三处注册；`remote/features/mod.rs` 加 `mod ssr;`。
7. 面板：`ibl.rs` 加「屏幕空间反射（SSR，10.3）」小节（`ui.strong` + checkbox + `ui.monospace("BRP: ash_renderer/ssr_set / ssr_stats")` + `ui.small("关 = 不录拷贝/步进/合成三段…")` + `ui.add_enabled_ui(enabled, …)` 滑杆组 + `ui.small("单次命中、离屏不可见…")`）；`overlay/tool_window/mod.rs` 加 `reflection_settings` 字段与页分发实参；`init.rs` 资源表登记。
8. `layout_registry.rs`：`ALL_PIPELINES` 追加三条（copy / march / composite）、`ALL_SETS` 追加两条（storage / sample）。
9. **出口**：`perf_stats` 有 `gpu_ssr_ms`；`ssr_set` 能开关与调参；`ssr_stats` 关档返 `enabled: false`；面板开关与截图一致；`layout_contract` 通过。

### 10.3.6 标定场景、判定线与取证

1. 标定场景（BRP 造，不改引擎）：一条自发光长条或亮色箱 + 一块低粗糙度地面/金属球（`spawn_shape` 已有 emissive 字段），三机位固定在标定物旁。
2. 采法：开/关各一张同机位、同曝光（`exposure_set {"mode":"manual"}`）、同后链参数的截图；HDR 域用 `frame_dump`；读数用 `ssr_stats`/`perf_stats`。
3. 逐条跑 §4 的 7 组判定，结论落 `10.3.6-取证与判定.md`（每组：口径、读数、截图、通过/部分/不通过）。
4. **9.8 局限自检的口径订正**在收官记录里做（主计划 §7）：把「无 SSR/局部反射」改成「屏幕空间、单次命中、离屏不可见」并注明本段出处；立案阶段不改施工结论。
5. 段 README 收官：一句话、当前状态、任务与记录表、判定线结果表、代码验证线、材料与现场（照 10.2 段 README 结构）。

## 3. 边界：本段不主张什么

- 不做光追反射：9.4 求交器与射线基建虽已在，但反射走光追不在本段（归后续段）；本段的命中色全部来自屏幕空间。
- 不做平面反射（镜像相机）、不做反射探针、不做 SSGI。
- 不做多次弹射：反射里看不到“反射中的反射”，单次命中即止。
- 不做时域累积/TAA：半分辨率步进在移动时可能闪烁，本段用 `w`、边缘 fade 与粗糙度门控兜底；时域另立。
- 不改 SCGI 缓存与采样链（归 9.11）、不改 G-buffer 附件布局、不改透明层行为（透明像素不接 SSR，也不参与反射命中）。
- 不改 `FrameUniforms` 契约（352 B）：不为了让步进“更好写”而加视图矩阵。
- 不为全屏管线加 push 常量：那属于核心改动，本段用“march 走 push、合成不须参数”绕开（R12）。
- 不做水/湿地面材质模型：标定用的“湿地面”只是低粗糙度材质实例。
- 不做 10.5 的 IA 整理、持久化、快捷键与 A/B 切换框架；本段只加一节 UI 与两个 BRP 方法。

## 4. 判定线草案

先写判定线，后测；每条都要有读数或截图，未测的写“未测”而不是留空。

### 4.1 关档恒等

1. **判定 1**：`ssr_set { "enabled": false }`（默认态）下，与立案基线（`c27b1ae7` 构建）同机位、同曝光、同后链参数的三张截图逐字节一致（`.temp/10.3/` 下 diff 为 0），且 `frame_dump` 的 HDR 域差值全 0。
2. **判定 1b**：R5 的抽取本身不改变画面——开档但标定场景里放一面“IBL 高光纯镜面”（`w` 全 0 的区域）时，`mode 10`（IBL 高光）视图与抽取前逐字节一致；等价地，抽取前后关档截图逐字节一致。
3. **判定 1c**：`ssr_stats` 在关档态返回 `enabled: false`，不触发回读（日志里无 readback 行），且不因读数而开任何 GPU 段。

### 4.2 质量

4. **判定 2（镜面标定偏移）**：1600×900、标定物为自发光条 + 光滑金属球/低粗糙度地面时，反射像在屏幕上的位置与“几何镜像应有的位置”偏移 ≤ 2 px；倒影强度与解析预期同量级（±20% 以内，允许因半分辨率与 IBL 混合口径带来的偏差，但要在记录里写明偏差方向与原因）。
5. **判定 3（粗糙度门控生效）**：`perceptualRoughness > roughness_cutoff` 的表面 `w` 恒 0——`ssr_stats` 在粗糙区窗口里的 `hit_ratio` 为 0；同机位把门控值调大/调小各一次，倒影只在光滑区出现/消失，画面无“整屏反射”。
6. **判定 4（离屏与背面回退）**：至少 3 个机位中有一个“转身见离屏物体”、一个“贴墙近景”，这些像素 `w = 0`、回退到 IBL 高光，画面**无拉丝、无黑块、无边界亮线**（读图判定）。

### 4.3 性能与构建

7. **判定 5a**：Release、1600×900、RTX 2060、默认机位下 `ssr_ms`（= 拷贝 + 步进 + 合成）p50 ≤ 1.5 ms（主计划判定线第 5 条）。
8. **判定 5b**：同一机位开/关对比，`frame_ms` p50 增量 ≤ 2.0 ms（含多出来的 copy 与两次 HDR 布局切换）；`gpu_lighting_ms` 不因新点位而失真（`Ssr → Lighting` 段应接近“透明层 + barrier”的原有量级）。
9. **判定 5c**：`cargo check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 零失败零告警；无新 crate 依赖（`Cargo.toml` 不变）。

### 4.4 契约不倒退

10. **判定 6a**：运行期零 `VUID` / `VK-ERROR`，覆盖:启动、resize、连续开关 `ssr_set`、滑杆连续拖动、连续 `ssr_stats` 回读。
11. **判定 6b**：BRP `ash_renderer/layout_contract` 通过（三条新管线与两个新 set 已登记，无未用 set、无重名）；实际读数（管线数与 set 数）在记录里回填，本计划不预设数字。
12. **判定 6c**：既有判定线不倒退——`gi_test`、`ssao` 相关读数与画面、`frame_dump` 的既有域、10.1 的 `rings` 四段和、10.2 的 AO 对比，均不回退（抽测，回归脚本沿用 `.temp/10.1`、`.temp/10.2`）。
13. **判定 6d**：`ssr_set` 非法参数（空参、越界、类型错、未知字段）返回 `invalid_params` 且不改任何设置值；`ssr_stats` 的 `rect` 越界裁到画面内、裁空返 null。

### 4.5 取证留档

14. **判定 7**：至少 3 个机位的开/关成对截图 + 读数（`ssr_stats`/`perf_stats`）留在 `.temp/10.3/`（不入库），结论分“通过 / 部分 / 不通过”并写明口径；标定场景（自发光条 + 光滑地面/金属球）至少一组。

## 5. 前置闸门与施工顺序

- **闸门 0（立案后、开工前）**：本文件落盘；路线图三处（步骤总览行、当前位置、步骤 10 段表 10.3 行）与步骤 README 两处（状态段、施工顺序表 10.3 行）同步；建 `10.3-SSR/README.md` 任务面板骨架（状态 `⏳ 未开工`）；确认 `goal/10.3` 从 `c27b1ae7` 建。
- **闸门 1（写下第一行代码前）**：采基线并落 `.temp/10.3/baseline/`——三机位关档截图 + `frame_dump` HDR、`perf_stats` 的 `frame_ms` 与各段 p50、`layout_contract` 的 52/30、9.10 锚点画面；读数抄进 `10.3.1` 记录头部，作为判定 1/5 的比对基准。
- **每个任务收口**：任务面板打勾 → `cargo check` + `clippy -D warnings` 零告警 → 写 `10.3.K-<名>.md` 施工记录（做了什么、怎么验证、读数、遗留）→ 分组提交（`feat` 与 `docs` 分开，**不推送**）。
- **施工顺序**：10.3.1 → 10.3.2 → 10.3.3 → 10.3.4（两小步）→ 10.3.5 → 10.3.6。10.3.2/10.3.3 期间允许“关档可跑、开档未接完”；从 10.3.4 起每次改动都要复验判定 1 与 1b。
- **段收官**：判定线逐条结论 + 三处状态流转（路线图、步骤 README、段 README）+ 归档 bullet + **9.8 局限自检口径订正**（主计划 §7，只在收官记录里做）+ 功能流程导读（读哪几个文件能读懂本段）。

## 6. 取证命令与产物

- 目录：`.temp/10.3/{baseline,shots,logs,evidence}/`；证据不入库（`.temp/` 已被忽略），只在记录里写“取哪个、看什么、结论是什么”。
- 工具：`tools/brp.py`（发 BRP 命令）、`tools/capture_window.py`（`PrintWindow` 抓帧）；开发实例端口沿用 `15703`（`ASH_BRP_PORT` 覆盖）。
- 典型命令：`ssr_set {"enabled":true,"intensity":1.0}`、`ssr_stats {"rect":[0,0,400,300]}`、`perf_stats`、`frame_dump {"domain":"hdr"}`、`exposure_set {"mode":"manual"}`、`layout_contract`。
- 截图判定按《画面功能视觉验证》：`PrintWindow` 截取 + 会话模型读图，先采后判；单张图不看结论，至少成对（开/关）与三机位。

## 7. 风险、备选与回退

- **风险 A：掠射角拉丝/拖尾**。步进在近掠射时命中点跳动。备选：命中置信度按“步长与深度差之比”衰减、步长随距离线性增大、边缘 fade 加宽；仍明显则把 `max_distance_m` 收紧（宁可少反射不拉丝）。
- **风险 B：拷贝插入后透明层黑帧**。合成写完 HDR 到透明层之间多了一次往返，布局或执行依赖漏一处就会丢内容或读到未写。对策：写出终点布局的显式断言（合成 → 可采样 → 附件写回 → 透明层），首帧就查 `VUID`；对照 `record.rs:328-360` 既有的同型 barrier 写法。
- **风险 C：IBL 高光扣多了/扣少了**，表现为金属变暗或仍然双份高光。对策：判定 1b 先把“抽取恒等”钉死；再单独用“只加不扣”的中间态（R13）确认命中色本身正确；最后才开减项。
- **风险 D：两帧在飞时同槽资源被覆写（WAR）**。每槽一份 `hdr_copy`/反射图/描述符集（照 10.2 R2 的同型结论），提交成功后才 `mark_written(slot)`；出现闪烁先查 slot 与提交流程，**不加 `device_wait_idle`** 掩盖。
- **风险 E：`ssr_stats` 回读打断渲染**。回读只在 BRP 调用时发生、抽样、关档不读；每次回读都要迁布局再恢复（`readback_image_region` 已如此）。
- **风险 F：管线/布局生命周期**。三条管线与两个布局照 SSAO 的建/拆顺序（`init_chain` 创建、`Drop` 释放、必要时 `device_wait_idle` 前置），`layout_contract` 测试兜底漏登记。
- **风险 G：半分辨率反射边缘抖动/锯齿**。首版接受（不做时域累积），用边缘 fade 与 `roughness_cutoff` 兜底；若判定 2 的偏移与判定 4 的画面都不达标，再把反射图提到全分辨率（成本约 ×4 的合成读带宽，步进仍半分辨率）。
- **回退**：任一判定失守且当场修不动时，保持 `ReflectionSettings::default().enabled = false`（关 = 基线），保留代码、读数与记录，把问题写进段 README 的遗留清单，按“部分通过”收官——不删代码、不改判定线去凑通过。

## 8. 文件与锚点清单

**新增**

| 文件 | 作用 |
|---|---|
| `src/render/passes/reflection/mod.rs` | `ReflectionSettings` 资源 + 模块对外导出 |
| `src/render/passes/reflection/reflection_targets.rs` | 每槽 `hdr_copy` + 半分辨率反射图、布局/池、`ensure`、两条描述符集 |
| `src/render/passes/reflection/reflection_copy_pipeline.rs` | HDR 拷贝全屏管线 |
| `src/render/passes/reflection/ssr_pass.rs` | 步进 compute 管线、`ReflectionPipelines`、`ReflectionFrameDraw` |
| `src/render/passes/reflection/reflection_composite_pipeline.rs` | 反射合成全屏管线 |
| `src/driver/host/frame/reflection.rs` | `prepare_reflection(...)`、`ReflectionFrameLocal`、push 常量打包 |
| `shaders/scene/reflection/reflection_copy.wgsl` | 全屏拷贝 |
| `shaders/scene/reflection/ssr.wgsl` | 步进核心（`cs_main`） |
| `shaders/scene/reflection/reflection_composite.wgsl` | 反射合成（`fs_main`，含 IBL 高光重算与减项） |
| `src/remote/features/ssr.rs` | `ssr_set` / `ssr_stats` |
| `.agents/docs/开发步骤/10-表现与使用体验优化/10.3-SSR/README.md` | 段任务面板与收官记录（开工闸门 0 建） |
| `.agents/docs/开发步骤/10-表现与使用体验优化/10.3-SSR/10.3.K-*.md` | 逐任务施工记录 |

**修改**

| 文件 | 改动 |
|---|---|
| `src/render/core/target/color.rs` | 加一条允许“storage + sampled + transfer”的构造（R6），不改既有两条 |
| `shaders/common/ibl.wgsl` | 加 `ibl_specular_from(...)`（R5） |
| `shaders/scene/gbuffer/gbuffer_composite.wgsl` | `ibl_environmental` 改为调 `ibl_specular_from`，调用位置与顺序不变 |
| `src/render/frame/record.rs` | 合成后插入“拷贝 → 步进 → 合成”三段与 `mark(Composite)`/`mark(Ssr)` |
| `src/render/frame/render_features.rs` | 加 `reflection: Option<ReflectionFrameDraw>` |
| `src/driver/host/frame.rs` | 解构新 input、`prepare_reflection`、尺寸重建参数、features 组装、`mark_written` |
| `src/driver/host/frame/input.rs` | 加 `reflection_targets`/`reflection`/`reflection_local` 字段 |
| `src/driver/host/frame/resize_gate.rs` | `rebuild_size_dependent` 加反射目标重建 |
| `src/driver/host/init.rs` | 目标/布局/管线创建与资源表登记 |
| `src/render/layout_registry.rs` | `ALL_PIPELINES` +3、`ALL_SETS` +2 |
| `src/render/stats/perf.rs` | `QUERIES_PER_FRAME = 15`、`Composite`/`Ssr` 点位、`fetch` 11 维、`gpu_ssr_ms` |
| `src/driver/host/frame/stopwatch.rs` | `gpu_sections` 11 槽与映射 |
| `src/remote/census/perf.rs` | `gpu_ssr_ms` 汇总与逐样本 |
| `src/remote/features/mod.rs` | `mod ssr;` |
| `src/remote/mod.rs` | 文档表、`use`、两条 `with_method_main` |
| `src/overlay/tool_window/ibl.rs` | SSR 小节 |
| `src/overlay/tool_window/mod.rs` | `reflection_settings` 参数与页分发 |
| `src/render/mod.rs` | re-export 新类型 |
| `.agents/docs/开发步骤/学习目标实现步骤.md` | 步骤总览、当前位置、步骤 10 段表（立案后） |
| `.agents/docs/开发步骤/10-表现与使用体验优化/README.md` | 状态段与施工顺序表 |
| `.agents/docs/开发步骤/10-表现与使用体验优化/施工计划：表现与使用体验优化三段拆解.md` | §2.3 段计划指针与状态 |
| `.agents/docs/开发步骤/学习目标执行记录归档.md` | 立案 bullet |

## 9. 与相邻段的关系

- **与 10.1（帧耗时先导）**：复用 `rings` 四段打点的写法与 `perf` 的“追加点位不重排”口径；本段加两点（`Composite`/`Ssr`），不改既有四段语义与 `rings` 读数口径。
- **与 10.2（SSAO）**：复用它的目标管理模式（布局进进程级、`ensure` 只换附件、每槽一份、关档为 `None`、`READBACK_STRIDE` 抽样回读）；10.2 的 `ssao_set`/`ssao_stats` 是本段 BRP 的模板。本段不改 SSAO 段，也不改合成里 AO 的乘点。
- **与 10.4（阴影减负）**：互不依赖。本段的标定场景若需要阴影配合，用现有阴影档位，不为 SSR 调阴影。
- **与 10.5（Egui 整理）**：本段只在 IBL 页加一节，IA 分组、持久化、快捷键、A/B 框架都留给 10.5；`ssr_set` 的参数名与语义一旦落定，10.5 直接挂。
- **与 9.7/9.10（SCGI）**：不改 SCGI 缓存、注入与采样链（归 9.11）；本段读的是 HDR 副本与 G-buffer，与 SCGI 无数据耦合。
- **与 9.8/9.11**：9.8 局限自检第 3 条「无 SSR/局部反射」由本段收窄为「屏幕空间、单次命中、离屏不可见」，订正**在收官记录里**做（主计划 §7），立案阶段只登记、不改结论。
- **与步骤 8（G-buffer）**：只读 G-buffer（depth/normal/matparam/albedo），不改附件布局、不改 `GBUFFER_SAMPLED_SET_DESC`。
