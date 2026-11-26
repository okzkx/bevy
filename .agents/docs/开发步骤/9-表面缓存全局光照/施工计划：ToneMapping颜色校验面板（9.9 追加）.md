# 施工计划：9.9 ToneMapping 颜色校验面板

> 2026-11-26 立案（纯文档，未开工；2026-11-26 用户增补段，编号追加在 9.8 之后）。对应[步骤 9 README](README.md)、[八段拆解 §2/§7](施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../学习目标实现步骤.md)。色调映射现状见路线图 8.6 与[Filament 色调映射参考](../../笔记/光照与材质/Filament%20色调映射参考.md)；自动曝光现状见[9.7.9 自动曝光](9.7-屏幕gather与注入/9.7.9-自动曝光.md)。本文只管 9.9 段做什么、按什么顺序、验收什么；开工后在该段目录（`9.9-ToneMapping颜色校验面板/`，开工时立）另立 `9.9.K-*.md` 施工记录，不回写本文。
>
> **当前状态：⬜ 已立案（2026-11-26 用户增补），未开工。** 任务 9.9.1～9.9.8 为待施工拆解（9.9.8 为 2026-11-26 第二轮追加：校色强度可手动/BRP 调整）；前置闸门见 §3、判定线草案见文末。

**一句话说明**：给现成的后处理链（Bloom → 曝光 → ACES/Narkowicz → LDR）加一段只读的 GPU 直方图与一个 egui 颜色校验面板，把"原始 HDR / 校色输入 / 校色输出"三个点的亮度与 RGB 分布画出来、把曝光参数控件补到面板上，读数同时从 BRP 可取——为 9.10 审计修复和 9.8 动态验收提供"颜色变了没有"的读数依据；本段不新增调色算子，不改 tonemap 曲线与曝光算法，直方图只读不进绘制决策；另加一个可手动与 BRP 调整的**校色强度**（曲线末端混合系数，0 = 不校色、1 = 完整曲线，默认 1.0 时输出逐位不变）。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-26 现状核实清单（本 checkout，逐项有出处）

| 项 | 结论 | 依据 |
|---|---|---|
| 后处理链顺序 | 曝光测光/适应 → Bloom 链 → tonemap（写 LDR 并转可采样）→ SMAA edge/blend → neighborhood | `ash_renderer/src/render/passes/post/post_chain.rs:42-105` |
| 校色三点定义 | present 里 `hdr` 加 Bloom、乘曝光倍数、经曲线；`mode!=0` 或 tonemap 关时直出 HDR（没有校色输出） | `ash_renderer/shaders/post/present_tonemap.wgsl:221-242` |
| 两条曲线 | `curve<0.5` 用 Narkowicz（`:38-45`），否则完整 ACES（`:163-219`，含 Rec2020↔sRGB；`bloom_params.gain` 就是 ACES 的 brightness） | 同文件；`ash_renderer/src/render/passes/post/bloom.rs:137` |
| HDR 目标格式 | RGBA16F（8 B/px；1600×900 ≈ 11.5 MB，全帧回读不可行） | `ash_renderer/src/render/core/target/color.rs:11` |
| LDR 目标语义 | RGBA8 UNORM，存**线性**值；sRGB 编码只在最后写 swapchain 的 SRGB view 时做一次 | `ash_renderer/src/render/passes/post/smaa.rs:127`、`post_chain.rs:61-63` |
| 已有测光读数 | 曝光 pass 已算 64×36 tile × 4×4 采样（36 864 样本）的 log2 亮度均值，`state[0] = (adapted, scale, avg_log2, 1)`；BRP `exposure_get` 可取 | `ash_renderer/shaders/post/exposure.wgsl:92-107`、`ash_renderer/src/render/passes/post/exposure.rs:391`、`ash_renderer/src/remote/features/render_settings.rs:99-193` |
| 曝光设置 | `ExposureSettings{mode, key, min_scale, max_scale, rate_brighten, rate_darken}`，默认 Auto / 0.01 / 0.25 / 4.0 / 0.7 / 3.0；无 CPU 侧读数镜像 | `ash_renderer/src/render/passes/post/exposure.rs:119-157`、`:210` |
| 无可视化 | 全仓 `histogram`/直方图零命中；overlay 现有 10 页无颜色页 | 全仓 grep；`ash_renderer/src/overlay/tool_window/` |
| 曝光无 egui 控件 | 只能用 BRP `exposure_set`/`exposure_get` | `ash_renderer/src/remote/features/render_settings.rs:99-193` |
| 现有 tonemap 控件的位置 | 「管线」页：Bloom 开关/强度（0.0–0.25）、tonemap 开关、曲线 radio、亮度增益滑杆 **0.5–2.5** | `ash_renderer/src/overlay/tool_window/pipeline.rs:31-56`（滑杆 `:47`） |
| 增益默认值 | `BloomSettings::gain` 默认 **8.0**，与滑杆上限 2.5 不一致：滑杆够不到默认值 | `ash_renderer/src/render/passes/post/bloom.rs:158-177`、`pipeline.rs:47` |
| 加页方式 | 新建 `tool_window/<page>.rs`，在 `ToolPage` 枚举、`GROUPS`、`show` 分发三处登记；已有面板用 `allocate_painter` 手画曲线，无 `egui_plot` 依赖 | `ash_renderer/src/overlay/tool_window/mod.rs:46-58`、`:77-91`、`monitor.rs:96` |
| LDR 回读现成 | `readback_ldr` 一次性 staging + fence，≤1 帧陈旧；`frame_dump` 的 PPM 取证也走它 | `smaa.rs:462`、`ash_renderer/src/remote/census/frame_dump.rs` |
| 缓冲回读现成 | `readback_buffer(ctx, src, out)`：一次性拷贝 + fence | `ash_renderer/src/render/core/device/readback.rs:16` |
| 描述符集分阶段 | present 的四个集（帧 UBO / G-buffer 采样 / Bloom 升采样 / 曝光状态）都是 FRAGMENT 阶段；compute 不能复用，需另建集 | `ash_renderer/src/render/passes/post/present_tonemap_pipeline.rs`、`ash_renderer/src/render/passes/post/smaa.rs:58-86` |
| shader 公共片段 | `// #import <name>` 只展开 `shaders/common/<name>.wgsl`，名字限 `[A-Za-z0-9_]`，公共片段不作入口编译 | `ash_renderer/build.rs:11-15`、`:129-176` |
| 8.6 的边界 | 只替换曲线；grading 全链与 EVILS 不主张（按需另案）、PBR Neutral 不做 | `.agents/docs/开发步骤/学习目标实现步骤.md:246` |

### 0.1 口径订正一：量的是"链上三点"，不是"帧缓冲"

- **域 A 原始 HDR**：`hdr_tex` 本身，未加 Bloom、未乘曝光——9.6/9.7 的缓存产物与直接光都在这里。
- **域 B 校色输入**：与 present `fs_main` 完全同式的 `(hdr + bloom * strength) * exposure_scale`（`present_tonemap.wgsl:228-235`），即曲线真正的输入。
- **域 C 校色输出**：tonemap 写进 LDR 的**线性** UNORM 值（sRGB 编码发生在 swapchain view，`post_chain.rs:61-63`）；面板显示时自己做一次 sRGB 编码。
- 为什么三域并列：只读 LDR，"曝光调错"与"曲线调错"混在一起；只读 HDR，又看不到钳位。三点并列才能回答"哪一步把颜色弄坏了"。

### 0.2 口径订正二：亮度轴用绝对 log2，与曝光读数同量纲

- A/B 两域：`luma = dot(rgb, LUMINANCE)`，权重与 `exposure.wgsl` 一致（0.2126/0.7152/0.0722），floor `1e-6`；`EV = log2(luma)`。
- 轴 −16…+8 EV，256 bin，0.09375 EV/bin；与 `exposure_state.z`（`avg_log2`）可直接比（判定线⑤）。
- C 域是显示域：x = sRGB 编码后的 0…1 值，256 bin；RGB parade 同轴；另计 0/1 钳位比例。
- 柱高用 `log10(count+1)` 压缩（面板内），读数与 CSV/BRP 用原始计数，保证可复核。

### 0.3 形态参考与裁剪

- 取市面调色 scopes 的三种基本形：亮度直方图（整体曝光与钳位）、RGB 三通道分布（偏色）、色度分布/矢量示波器（饱和度与色偏）；再加"钉住基线"做 A/B 对比——实时引擎的运行时调试面板也以"当前帧读数 + 基线"呈现。
- 裁掉：波形图（waveform）、假色、斑马纹、逐像素取色（Pixel Inspector 类）、录屏/导出视频——都不是"端到端校验 tonemap"的最小集。
- 定性：这是**读数工具**，不是新的调色界面；调色算子的增删按 8.6 的裁决走，不在本段。

### 0.4 面板与 BRP 的分工

- egui 面板：人看，8 Hz，只在面板展开时采样；关闭时零 dispatch、零回读。
- BRP `color_scope_get`：脚本/回归，取最近一次样本（带 `frame_age`/`stale`），可按需强制下一帧重采；字段与面板同源。
- 两边不各算一份：CPU 只解析同一份 bins，面板不另立统计口径。

### 0.5 边界（本段不主张什么）

- 不加/不改调色算子：白平衡、对比度、饱和度、色轮、LUT、EVILS、PBR Neutral 都不做（8.6 已裁）；两条曲线的数学与曝光测光/适应算法不改。
- 只新增**一个可写参数**：校色强度（`tonemap_strength`，曲线末端 `mix` 系数，0 = 曝光后线性钳位、1 = 完整曲线）；默认 1.0 时输出与立案基线逐位一致；不做"用强度反推曝光/曲线参数"。
- 直方图只读：不参与曝光决策、不回写设置、不参与自动曝光；强度滑杆改的是渲染参数，不是直方图。
- 不做 HDR 全帧回读；不做 GPU 端百分位（CPU 从 bins 插值，轴精度 1/256）。
- 不修 9.10 审计的问题（9.9 只记录读数）；不做显示域（SDR/HDR 输出、色域）比较；不做图片/视频导出。
- 不引入 `egui_plot` 等新依赖；不新增常驻纹理/附件。

## 1. 数据流总览

```
                 ┌─ A 原始 HDR ────── luma + RGB（log2 轴）直方图
hdr_tex ─────────┤
                 ├─ +bloom*strength ─ ×exposure_scale ─ B 校色输入 ─ luma + RGB（log2 轴）
                 │
                 └─ tonemap(curve) → LDR（线性 UNORM）─ C 校色输出 ─ luma + RGB parade（sRGB 轴）
                                                                  + CbCr 64×64 + 钳位计数
                                   bins SSBO（约 29 KB/样本）
                                     ├─ egui「颜色」页（8 Hz，展开时才跑）
                                     └─ BRP color_scope_get / CSV（按需）
```

- 录制点：`post_chain.rs:42` 的 `record_to_ldr` 内、tonemap 出场屏障之后（LDR 已 `SHADER_READ_ONLY`）、SMAA edge 之前；`frame.mode != 0` 或 `bloom_params.tonemap < 0.5` 时**跳过**（present 此时直出 HDR，没有校色输出），面板显示"链未启用，无 C 域"。
- 采样网格：每 2 像素取 1（stride 2，1600×900 约 36 万样本）；复用 present 的 `blit_sampler`/`bloom_sampler` 做线性采样，保证与 present 同式。
- bins 布局（`u32`）：

| 区 | 内容 | u32 数 |
|---|---|---|
| 头 | magic / stride / 尺寸 / 帧号 / 本帧曝光状态 4 个 f32（adapted, scale, avg_log2, 占位） | 16 |
| A | luma 256 + RGB 3×256 + 计数 5（样本数/低于轴/高于轴/0/1） | 1 029 |
| B | 同 A | 1 029 |
| C | luma 256 + RGB parade 3×256 + 计数 5 | 1 029 |
| C 色度 | CbCr 64×64 | 4 096 |
| 合计 | | 7 199（≈ 28.8 KB） |

（C 域 luma 与 parade 都在 sRGB 编码后的 0…1 轴；A/B 在 log2 轴。）

- 校色强度（§9.9.8）只改 present 的曲线输出，因此只影响 C 域与最终画面；A/B 两域与 bins 布局不变。

## 2. 任务拆解

### 9.9.1 公共片段抽取 + scope 管线基座

- 把 present `fs_main` 里"Bloom 叠加 + 曝光乘"两步抽成 `ash_renderer/shaders/common/tonemap_shared.wgsl`（纯函数：输入 hdr/bloom 颜色与参数，输出曲线输入色；**不含曲线、不含 `mode` 直出分支**），`present_tonemap.wgsl` 与新的 `ash_renderer/shaders/post/color_scope.wgsl` 共用；`// #import tonemap_shared`。
- 新 compute 管线（参照 `exposure.rs` 的 Targets/Pipelines 形状）：`COLOR_SCOPE_SET_DESC`（COMPUTE 阶段）绑定 hdr 采样图 + `blit_sampler` + bloom 采样图 + `bloom_sampler` + Bloom 参数 UBO + 曝光状态 UBO + LDR 采样图 + bins SSBO；每帧用既有视图/缓冲重建该集（引用同一份资源，不复制状态）。
- `PostChainDraw` 增加可选 `color_scope` 字段；按 §1 的跳过条件与节流决定是否录制；bins 缓冲用 `STORAGE | TRANSFER_SRC`，另配 staging。
- 验收：抽取前后同一帧 `frame_dump` PPM 逐像素一致；关闭 scope 时命令序列与帧时间回立案基线；零 VUID。

### 9.9.2 三域采样与直方图

- `color_scope.wgsl`：`@workgroup_size(8,8)`，每线程取一点，按 §0.1/§0.2 算三域；A/B 记 luma（log2 轴）与 RGB（log2 轴），C 记 sRGB luma、RGB parade 与 CbCr 64×64；同时累加五类计数（样本数、低于轴、高于轴、0、1）。
- 首版直接 `atomicAdd` 到 bins（stride 2，约 250 万次原子）；实测超预算再退到共享内存合并或 stride 4（§5 待决一）。
- 头区写本帧曝光状态快照（从曝光状态 UBO 取 4 个 f32），面板/BRP 不另外回读曝光缓冲。
- 验收：常量天空 `sky_uniform` 自检模式下 A/B 峰值落在理论 EV ±0.1；灰卡/纯色测试图下 RGB 三通道峰值重合、色度分布落在对应色点。

### 9.9.3 回读与节流

- bins → staging（`readback_buffer`，一次性拷贝 + fence，`readback.rs:16`），CPU 解析成快照（原始计数 + 统计量：均值、p1/p50/p99、钳位比例、样本数、`frame_age`）。
- 触发：面板展开时按时间 8 Hz；BRP 请求时下一帧强制一次；面板收起后停止（保留最后快照并标 `stale`）。
- 验收：不展开/不请求时 GPU 无 dispatch、无回读；8 Hz 下无可感卡顿（判定线⑨记录帧时间）；同帧面板与 BRP 快照逐字段一致。

### 9.9.4 egui「颜色」页

- `ToolPage::Color` 进「渲染」组（`tool_window/mod.rs:46-58`、`:77-91`）；新建 `ash_renderer/src/overlay/tool_window/color.rs`。
- 版式：顶部状态行（帧号/样本数/`stale`/是否跳过）+ 钉住基线按钮 + Δ 摘要；三段折叠（A/B/C），每段 luma 柱状（log-y）+ RGB 三线叠画 + 统计表；C 段额外 sRGB 直方图与 CbCr 64×64 热图（`egui::ColorImage` 上传一次纹理，更新时 `set`）；底部曝光/曲线/校色强度控件。
- 钉住：快照拷贝到 CPU 侧 `PinnedBaseline`，显示"当前 − 基线"的均值/钳位差；不额外采样。
- 验收：读图（§3 闸门③）三域齐全、轴标注与单位可读；收起面板后 1 帧内停止采样（BRP 的 `frame_age` 增长可见）。

### 9.9.5 曝光/曲线控件补面 + 增益滑杆修正

- 颜色页加 `ExposureSettings` 控件：Auto/Manual、`key`、`min_scale`/`max_scale`、`rate_brighten`/`rate_darken`；读数用 §9.9.2 头区的快照（与 BRP `exposure_get` 同值）。手动模式只切模式，不承诺 EV 偏移（现状 scale 恒 1，`present_tonemap.wgsl:234`）。
- 修 `pipeline.rs:47` 的增益滑杆范围：至少覆盖默认 8.0（建议 `0.5..=16.0`），文案写清"ACES 内亮度增益（1.0 = 纯 ACES，1.667 ≈ ACES_LEGACY）"。
- 验收：改任一控件后下一帧面板读数变化、`exposure_get`/`bloom_set` 同步；滑杆能取到 8.0 默认值；`Manual` 下 B 域均值只随画面变、不随 `key` 变。

### 9.9.6 BRP `color_scope_get` 与复核脚本

- `ash_renderer/src/remote/features/color_scope.rs`（一文件一功能，在 `remote/features/mod.rs` 登记）：返回快照 JSON——头区（帧号/年龄/曝光状态）、三域统计、三域 luma 256 箱、A/B RGB 256 箱（可按需省略）、C 的 CbCr 16×16 下采样；可选 `dump` 路径写完整 CSV（三域全部 bins），供 `tools/color_scope_check.py` 复核。
- 脚本：读 CSV 重算均值/百分位/钳位，与 JSON 统计量互校；对常量输入做逐 bin 相等断言。
- 验收：BRP 与面板同帧一致；脚本在常量天空与纯色测试图上通过；真实场景允许 ≤2% 采样差异（记录）。

### 9.9.7 判定线、读图留档与施工记录

- 按文末判定线逐条取证；读图存面板展开/收起、Narkowicz/Filament、Auto/Manual 四组；成本三档（关面板 / 8 Hz / 30 Hz）记进 `9.9.K-判定线与计量.md`。
- 文档：段目录 README、施工记录；README/路线图段表状态回填；`9.7.9-自动曝光.md` 的"无直方图测光"局限不在此处改写（只做交叉引用，施工时定）。

### 9.9.8 校色强度：手动 + BRP 可调（2026-11-26 第二轮追加）

- 语义：`out = mix(exposure_color_clamped, tonemap(exposure_color), strength)`，`strength ∈ [0,1]`，默认 `1.0`（与现状逐位一致）；0 是"完全不校色"的对照视图，1 是完整曲线。只作用于 tonemap 真正执行的路径——`frame.mode != 0` 或 tonemap 关时仍直出 HDR，不受强度影响。
- 参数通道：`present_tonemap.wgsl` 的 `BloomParams` 追加第 6 个 `f32`（`tonemap_strength`）；UBO 仍 32 B（沿用原 3 个 pad 中的 1 个），Rust 侧 `BloomParamsData`/`BloomParamsLayout`/`pack_bloom_params` 在偏移 20 写入，`BLOOM_PARAMS_SIZE` 不变；`BloomSettings` 加 `tonemap_strength: f32` 默认 `1.0`。两条曲线分支先各自算出 `mapped`，再统一 `mix` 一次返回（Narkowicz 分支不再提前 `return`）。
- 手动：颜色页底部"校色强度"滑杆 `0.0..=1.0`（拖动时 8 Hz 读数立刻反映在 C 域直方图）；强度只放在颜色页一处，管线页保留原 Bloom/曲线控件、不重复放（避免两份状态，见 §5 待决六）。
- BRP：`bloom_set` 增键 `tonemap_strength`（数值，`0..=1`，非有限值/越界拒绝），回显 `applied`/`current`；同步 `render_settings.rs` 的请求注释与 `pipeline.rs` 的"BRP: bloom_set / smaa_set"提示。
- 验收：默认 1.0 时 `frame_dump` 与立案基线逐位一致；强度 0/0.5/1 三档各一张读图，C 域直方图随强度单调变化（0 时 C 域 ≈ 曝光后线性钳位分布）；egui 与 BRP 写同一份 `BloomSettings`，读回一致。

## 3. 前置闸门（开工首日按序关闭）

① 基线取证：默认场景同一帧 `frame_dump` + 帧时间 300 帧均值 + `exposure_get` 读数，存档为 9.9 起点。
② 抽取闸门：公共片段抽取后 present 输出逐像素不变（判定线①）；不通过就回退抽取、改为 scope 内联复制并记录理由。
③ 描述符/屏障核实：scope 读 LDR 时布局为 `SHADER_READ_ONLY`；compute 集含 STORAGE 缓冲与采样图，确认不与 SMAA/曝光的状态冲突。
④ 回读通道核实：`readback_buffer` 在现有帧槽 fence 体系下安全（一次性提交，不与后台帧交叠）。
⑤ 采样节流与面板生命周期核实：`ToolPage` 的当前页/折叠状态读取方式；面板未打开时必须不采样。
⑥ 强度参数闸门：`BloomParams` 增字段后 WGSL/Rust 布局一致（`shader_abi` 重新生成、零 VUID）、默认 1.0 时输出逐位不变；若布局不通过，把强度改放独立 UBO 而不扩 `BloomParams`。

## 4. 显存与性能口径

- 常驻：bins SSBO ≈ 28.8 KB + staging 2 × 32 KB ≈ 64 KB；不新增纹理/附件。
- 每样本 GPU：一次 dispatch（800×450 线程）、最多 3 次纹理读 + 约 7 次原子；目标 ≤0.3 ms @1600×900（8 Hz 时摊薄到 ≤2.4 ms/s）。
- CPU：解析 28.8 KB + 画 4 张图 @8 Hz，目标 <0.5 ms/次。
- 超预算处理顺序：stride 4 → 共享内存合并 → 降到 4 Hz；实测与选择写进施工记录。
- 校色强度只多写 4 B 到既有 Bloom 参数 UBO，无新增缓冲/纹理/采样/描述符。

## 5. 待决项（施工时钉明，不预设结论）

1. stride 2 vs 4、直接原子 vs 共享内存合并——按实测选，记录数据。
2. 亮度轴 −16…+8 EV 是否够：若"低于轴"比例 >5%，调轴并同步曝光对照口径。
3. C 域是否再存一份线性 log2 直方图（便于与 A/B 精确对位）；首版不做。
4. 曝光控件最终放颜色页还是管线页（首版颜色页；若与管线页重复再合并）。
5. BRP JSON 的 CbCr 分辨率（16×16 下采样 vs 完整 CSV dump）与体积上限。
6. 校色强度的落位与语义：首版按末端 `mix`、只放颜色页（末端 `mix` 更适合回答"校色有没有过火"；若手感更像"强度"，可改成缩放曲线输入，代价是黑位变化）；若要与曝光控件同页展示，迁移成本是一行绑定。
7. 是否允许强度 >1（超校色）：首版限 `0..=1`；>1 属外插、高光会过冲，不做。

## 6. 与前后段的接口

- 与 9.10：`color_scope_get` 是审计修复的复跑读数工具；9.10 改画面后应复跑本段判定线④⑤⑧⑩（只跑读数，不重做整段），其参数对照把 `tonemap_strength` 列为 9.9 新增的唯一可写校色参数（默认 1.0）。
- 与 9.8：9.8 的动态验收可直接引用面板/BRP 读数（曝光、钳位、色偏）；本段不改 9.8 范围。
- 与 9.7/9.5 调试视图：`frame.mode != 0` 时跳过 scope（调试视图直出 HDR）；不改 `frame.mode` 语义。
- 与 9.7.9 自动曝光：只读 `ExposureSettings`/状态，不改测光与适应算法；面板不写回曝光决策。
- 与 8.6：tonemap 曲线与 grading 边界不动；本段只加读数、控件与校色强度参数。
- 施工顺序：编号追加在 9.8 之后，但建议施工顺序 9.9 → 9.10 → 9.8；9.8 只是普通功能段（动态验收、压测与局限对照），不是本步收尾，施工完仍会有新任务追加，所以审计修复排在它之前只是为了让压测读到修好的画面，不代表 9.8 是终点。

## 判定线草案（段级汇总）

1. **抽取无损**：公共片段抽取前后同一帧 `frame_dump` PPM 逐像素一致；scope 关闭时命令序列/帧时间回基线。
2. **零采样开销**：面板收起且无 BRP 请求时 GPU 无 dispatch、无回读（`frame_age` 不增长 + 帧时间对比证明）。
3. **三域齐全**：A/B/C 直方图非空且统计表可读；`mode!=0`/tonemap 关时 C 段明确显示"链未启用"，无旧数据残留。
4. **轴自检**：常量天空 + 固定曝光下 A/B 峰值 EV 与理论值差 ≤0.1 EV；纯色测试图三通道峰值重合、CbCr 落点正确。
5. **与曝光对齐**：B 域均值 EV 与同一帧头区 `avg_log2` 差 ≤0.15 EV（记录实测差值，口径差异写明）。
6. **钳位正确**：全黑/全白测试图下 C 域 0/1 比例 = 100%（UNORM 量化导致的 99.6% 也接受，如实记录），A/B 的"低于/高于轴"计数与之相符。
7. **BRP 与面板一致**：同帧快照逐字段一致；CSV 复核脚本与 JSON 统计量互校通过。
8. **无副作用**：面板开关前后画面输出逐位一致（scope 只读）；校色强度保持默认 1.0 时输出与立案基线逐位一致；零 VUID、clippy 全净、`Cargo.toml` 无 diff。
9. **成本留档**：关面板 / 8 Hz / 30 Hz 三档帧时间与 GPU 成本写入施工记录；>0.3 ms 时按 §4 降级并复测。
10. **校色强度可调且同源**：egui 滑杆 0/0.5/1 与 BRP `bloom_set.tonemap_strength` 写同一设置、读回一致；三档读图与 C 域分布单调可辨；`bloom_set` 拒绝 0..1 外与非有限值。
