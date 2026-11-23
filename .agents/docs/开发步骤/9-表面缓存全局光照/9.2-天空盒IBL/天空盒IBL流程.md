# 天空盒 IBL 流程

本篇走读 9.2 引入的新机制：**环境光从"常量"变成"一张天空 cubemap 派生的基线"**。适合先看完整数据链、再看帧内时序；细节取舍见各任务施工记录，原理（SH、split-sum）见任务记录与笔记。

## 1. 一张图，两条派生结果

```
9.1 物理天空 sky_radiance(dir, include_sun=false)
        │  ② 段捕获（脏帧才跑）
        ▼
环境 cubemap 256²×6 RGBA16F 9 级（mip0 = 捕获源）
        ├── SH 投影（1024 方向 → 9×RGB，折叠 A_l）──→ storage buffer 144B
        ├── sh_check 两条自检 ──→ stats 64B
        └── GGX 预滤波（mip1..8）──→ 同一张图的 mip1..8
DFG LUT 128²（一次性）──→ A/B
        ▼  ⑤ 段
合成/透明前向：sh_irradiance(n) · diffuseColor/π + prefiltered(R) · (F0·A + B)
```

- **一张图两个角色**：`mip0` 是锐利源（SH 投影与预滤波都读它），`mip1..8` 是预滤波结果（高光采样）。同图靠子资源分区，捕获写 0 级、预滤波写 1..8 级。
- **两侧同源**：漫反射与高光读同一张源 cubemap，SH 与预滤波方向一致；DFG LUT 与天空无关，只生成一次。
- 这张 cubemap 是 **9.3 GlobalProbe 与 9.7 SCGI 的共同上游**：9.3 换的是漫反射编码（16 桶），9.7 只替换"不透明、Volume 内"的漫反射项，高光永远是 IBL。

## 2. 各级账面一览

| 资源 | 形态 | 写者 | 读者 |
|---|---|---|---|
| 环境 cubemap | 256²×6、RGBA16F、9 级、`CUBE_COMPATIBLE` | `sky_capture`（mip0）、`prefilter`（mip1..8） | `sh_project`/`prefilter`（mip0）、合成/透明（全链 cube） |
| DFG LUT | 128²、RGBA16F、1 级 | `brdf_lut`（一次性） | 合成/透明 |
| SH 缓冲 | storage 144B（9×vec4） | `sh_reduce` | 合成/透明（片段读）、BRP `ibl_stats`（拷贝读） |
| stats 缓冲 | storage 64B（l=0 + 14 法线误差） | `sh_check` | BRP `ibl_stats` |
| 中间缓冲 | `partial` 27KB / `radiance` 16KB | `sh_project` | `sh_reduce` / `sh_check` |
| 帧 UBO | 336B（+`ibl_*` 4 字 + `sky_uniform` 4 字） | `write_frame_ubo` | 所有 pass |
| 烘焙账本 | `captured_hash` / `last_capture` / `count` / `dfg_baked` | `prepare`（draw_frame） | `prepare`、`ibl_stats` |

## 3. 新文件分工（一层一句话）

| 文件 | 职责 |
|---|---|
| `src/render/passes/ibl/mod.rs` | 设置（`IblSettings`）、帧内录制束（`IblFrameDraw`/`IblBakeDraw`）、重出口 |
| `src/render/passes/ibl/environment.rs` | 资源族（图/视图/采样器/缓冲/六条管线/描述符集）、`prepare` 脏判定、`record_bake` 录制、诊断读回 |
| `shaders/common/ibl.wgsl` | 立方体面↔方向、SH 基与 `sh_irradiance`、GGX 重要性采样、split-sum |
| `shaders/scene/ibl/sky_capture.wgsl` | 天空 → mip0（六面一次 dispatch） |
| `shaders/scene/ibl/sh_project.wgsl` | 1024 方向投影 → partial + radiance 缓存 |
| `shaders/scene/ibl/sh_reduce.wgsl` | partial → 9 系数（折叠 `A_l`） |
| `shaders/scene/ibl/sh_check.wgsl` | 两条自检 → stats |
| `shaders/scene/ibl/prefilter.wgsl` | GGX 预滤波，逐 mip 派发 |
| `shaders/scene/ibl/brdf_lut.wgsl` | DFG LUT 一次性生成（A/B 钳到 1） |
| `src/remote/features/ibl.rs` | `ibl_set` / `ibl_stats` / `ibl_census` |

## 4. 逐时刻走读

### 4.1 帧首（draw_frame，Last）

1. `wait_for_slot`（本槽两帧前提交完成）→ `write_frame_ubo` 写本帧 336B UBO（含 `ibl_enabled/mode/intensity`、`sky_uniform`）。
2. `scene.ibl_env.prepare(&IblBakeInput{ sky, dir_to_light, light_color, altitude, enabled })`：
   - 首帧：`plan.init = true`（布局初始化 + 缓冲清零）；
   - `ibl_enabled=false` → 直接返回（后续每次只要 init 已做就返回空计划，② 段零成本）；
   - 算 `sky_hash`（SkySettings 全字段 + 太阳 + 100 m 量化海拔）→ 与 `captured_hash` 比对，加 0.1 s 节流；
   - 脏 → `plan.capture = true`（DFG 未生成再带 `plan.dfg = true`），账本先行（计数 +1）。
3. `RenderFeatures.ibl = IblFrameDraw{ bake, set }`：`bake` 为空计划时为 `None`；`set` **恒有**（两条管线恒声明 group 4）。

### 4.2 ② 段：烘焙链（脏帧）

1. 首次：cubemap/DFG 全子资源 `UNDEFINED→SHADER_READ_ONLY`；SH/stats `cmd_fill_buffer` 清零 + transfer→shader 屏障。
2. DFG（一次性）：`SHADER_READ_ONLY→GENERAL` → `(16,16,1)` dispatch → 回采样态。
3. 捕获：全子资源 `SHADER_READ_ONLY→GENERAL`（src = 上一帧的采样读，WAR）+ **缓冲跨帧 WAR 内存屏障**；`sky_capture` 写 mip0 六面。
4. mip0 单级 RAW（`GENERAL→GENERAL`）→ `sh_project`（1024 方向；`Handoff::ComputeRead`）→ `sh_reduce`（9 系数）→ `sh_check`（两条自检；交班到 `ComputeFragmentRead`）。
5. `prefilter` 逐级 mip1..8（push = p 与样本数，dispatch `(size/8,size/8,6)`）。
6. 链尾：`Handoff::ComputeFragmentRead`（缓冲）+ 全子资源 `GENERAL→SHADER_READ_ONLY`（图像）。

### 4.3 ⑤ 段：注入

- 合成 pass 绑 5 个 set（0 帧 UBO / 1 G-buffer / 2 光源分簇 / 3 阴影 / 4 IBL）；透明前向同理。
- 片段着色器：`ibl_environmental()` 先看 `frame.ibl_enabled`——关则两项为 0，`select` 回 `albedo·ambient`（逐位一致）；开则
  - 漫反射 `diffuseColor/π · E(n) · intensity`（`E` 已含余弦卷积）；
  - 高光 `prefiltered(R) · (F0·A + B) · intensity`（`R = reflect(−V,n)`，mip = p·8）；
  - 调试档 9/10 直出单项贡献。
- **9.2.8 的两处防噪**：IBL 项先把法线翻到视向侧（`ibl_normal`，双面几何背面不让 DFG 落到掠射端），再用屏幕空间法线导数把粗糙度撑开（`ibl_roughness`，Toksvig 近似）；DFG LUT 本身修了积分器（见 §6）。

### 4.4 稳态（不脏）

`prepare` 返回空计划 → ② 段一行 `if` 即返回（零屏障、零 dispatch）；合成/透明照常采样上一轮烘焙的 cubemap/SH；`ibl_stats` 读到的是"最近一次完整捕获"的值。

## 5. 失败与边界路径

| 情况 | 行为 |
|---|---|
| `ibl_enabled=false` | 不捕获、不读 IBL 项；等于步骤 8 基线（逐位一致） |
| `sky_enabled=false` + IBL 开 | 背景清屏色 + **环境项回退常量 `albedo·ambient`**（9.2.8 起天空是环境光上游：不捕获、`ibl_stats` 计数不增；不再有"有 IBL 无可见天空"） |
| 常量天空（`sky_uniform.w=1`） | 可见天空与捕获同时变成常量 `L`（不经 `sky_intensity`），`E(n)=πL` 可解析验收 |
| 太阳排除（三介质/地面=0） | SH 全 0、IBL 两项≈0（圆盘不进捕获） |
| 设备缺 `STORAGE_IMAGE`/线性过滤/5 个 set | 初始化报错退出（不静默降级） |
| 退化法线（`normalize(0)`） | `safe_normalize` 兜底 —— 不再让 NaN 经 IBL 把 Bloom 扩成黑块 |
| 捕获超时/参数抖动 | 0.1 s 节流；可见天空即时、IBL 最多滞后一个窗口 |

## 6. 代码改动简要解析

**新增**（`git show --stat dad5633c9` 实数：全段 37 文件 +2543/−76，其中新增文件 11 个）：

| 文件 | 要点 |
|---|---|
| `shaders/common/ibl.wgsl`（新） | 面方向表（Vulkan cube 约定）、`sh_basis`/`sh_band_factor`/`sh_irradiance`（A 折叠）、`importance_sample_ggx`、`hammersley`、`uniform_sphere`、`ibl_specular`；`IBL_MAX_MIP = 8` 与 Rust 侧同源 |
| `shaders/scene/ibl/*.wgsl`（新 6 个） | 捕获 / 投影 / 归约 / 自检 / 预滤波 / DFG；全部单入口 compute，组号 0 |
| `src/render/passes/ibl/{mod,environment}.rs`（新） | 资源族 + 六条管线 + 描述符集（单池 + 帧集独立池）+ `prepare`/`record_bake`/`readback_cube`/`readback_dfg`；`Drop` 序：管线 → 池 → 布局 → 采样器，视图先于图 |
| `src/remote/features/ibl.rs`（新） | `ibl_set`/`ibl_stats`/`ibl_census`；读数走一次性拷贝 |

**扩展**：

| 文件 | 改动 |
|---|---|
| `frame_uniform_table.rs` / `frame.wgsl` | UBO 336B、`ibl_*` 与 `sky_uniform`、五处同改 + `ubo_handle()` 只读口 |
| `sky.wgsl` | `sky_uniform` 常量分支（在 `sky_intensity` 之前返回） |
| `barrier.rs` | `ImageBarrier` 加 `base_mip/level_count/base_layer/layer_count`（默认旧行为）+ `transition()` + `with_range()` |
| `gbuffer_composite.wgsl` / `transparent_forward.wgsl` | group 4 四个绑定、`ibl_environmental`、`environment = select(ambient, ibl)`、调试档 9/10 |
| `composite_pipeline.rs` / `transparent_pipeline.rs` | 第 5 组 `IBL_SET_DESC` |
| `record.rs` | ② 段首 `feature.ibl.record_bake` + `IblCapture` 打点；⑤ 段绑 IBL 集 |
| `render_features.rs` / `draw_frame` | `IblFrameDraw` 束；`prepare` 在 UBO 写入之后按灯光/海拔判脏 |
| `init.rs` | `IblEnvironment` 进创建链与拆除序（在 `FrameUniformTable` 之前拆） |
| `perf.rs` / `stopwatch.rs` / `census/perf.rs` | 9 位 timestamp、`gpu_ibl_capture_ms` |
| `render_mode.rs` | 11 项（`ibl_diffuse`/`ibl_specular`） |
| `brdf.wgsl` / `gbuffer_write.wgsl` / `transparent_forward.wgsl` | `safe_normalize` 兜底（退化法线 NaN 的源头修复） |
| `brdf_lut.wgsl` | A/B 钳到 1（NoV→0 发散 → fp16 Inf → Bloom NaN 的修复）；**9.2.8 修积分器**：`G = 4·NoL·NoV·V` 再除 `NoH·NoV`，钳 1 降为数值护栏 |
| `shaders/common/ibl.wgsl`（9.2.8） | `ibl_normal`（视向侧法线）+ `ibl_roughness`（屏幕空间 specular AA），不透明/透明共用 |
| `gbuffer_composite.wgsl` / `transparent_forward.wgsl`（9.2.8） | IBL 项改用上述两个辅助函数（直接光不动） |
| `shaders/common/sky.wgsl`（9.2.8） | 海拔按 1 m 兜底 + 球求交稳定式 `d = c² − k`（地平线以下椒盐噪点） |
| `driver/host/frame/scene.rs` / `frame.rs`（9.2.8） | `ibl_enabled` 与捕获条件都乘 `sky.enabled`（天空关 → 常量 ambient） |
| `src/remote/features/ibl.rs`（9.2.8） | `ibl_census` 加 `row`：读回 DFG 单行 128 个 A/B |
