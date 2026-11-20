# 施工计划：9.2 天空盒 IBL

> 2026-11-20 立案（纯文档，重规划）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.2](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.2 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：📝 施工计划已立案，未开工（2026-11-20）**。

**一句话说明**：把 9.1 的程序化天空捕获为环境 cubemap，投影 SH-L2 漫反射并预滤波高光，替换不透明与透明的常量环境项；本段不产生世界空间缓存、射线或 SCGI。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.1 两层语义：IBL 是新的环境基线，SCGI 只替换不透明漫反射

从本段起，场景的环境光分两层：

```
IBL（9.2）：所有受光材质的环境基线——不透明 + 透明；天空变化时重烘
SCGI（9.7）：只替换"不透明像素、在 Volume 内"的漫反射间接项；体积外回退 IBL
```

- `ibl_enabled = false` 时，不透明与透明都回到步骤 8 的 `albedo * ambient_color`，逐位一致；这是回归硬线。
- `ibl_enabled = true` 时，环境项被 IBL 的漫反射 + 高光**替换**（不是叠加）；透明层也接 IBL，因为它是新的环境基线，不是 GI。
- SCGI 的 `gi_enabled` 到 9.7 才出现；它只影响不透明合成，不影响透明层。
- 背景天空归 9.1 的 `sky_enabled`，与 `ibl_enabled` 正交：可以"有天空无 IBL"（对照），也可以"有 IBL 无可见天空"（只测材质）。
- `ibl_enabled` 段内默认关（BRP 开），避免 9.2 一落地就改变所有场景的基线；9.8 验收后再拍板是否默认开。

### 0.2 数据链：一张源 cubemap，三个派生结果

```
sky_radiance(dir, include_sun=false)          // 9.1 的函数，捕获时排除太阳圆盘
        │
        ├── 环境 cubemap（256²×6 RGBA16F，mip 链，源）
        │        ├── SH-L2 投影 → 9×RGB 系数 → storage buffer   （漫反射）
        │        └── GGX 预滤波 → 预滤波 cubemap（mip 链）      （高光）
        └── DFG/BRDF LUT（128²，一次性生成）                    （高光）
```

- **捕获排除太阳圆盘**：太阳是小立体角高频亮斑；进 SH 会污染低频项，进预滤波会和直接光的高光双计。可见天空仍画圆盘（9.1 的 `include_sun=true`）。
- 源 cubemap 只存 `sky_radiance(dir, false)`；太阳的直射与高光由现有的直接光 `surfaceShading` 负责。
- 漫反射与高光读同一张源 cubemap，保证两者方向一致；DFG LUT 与天空无关，只在初始化生成一次。

### 0.3 漫反射：SH-L2 + 折叠余弦卷积

标准 IBL 的漫反射用 L2（9 系数/通道），不是 L1。L1 只能表达法线的线性梯度，L2 能表达地平线带与太阳侧的非对称；对天空这种低频但有大尺度结构的函数，L2 的误差约 1%，是行业默认。

**基函数（real SH，Y-up）**：

| 系数 | 基函数 |
|---|---|
| `Y00` | `0.28209479` |
| `Y1-1` | `0.48860251 · y` |
| `Y10` | `0.48860251 · z` |
| `Y11` | `0.48860251 · x` |
| `Y2-2` | `1.09254843 · x · y` |
| `Y2-1` | `1.09254843 · y · z` |
| `Y20` | `0.31539157 · (3z² − 1)` |
| `Y21` | `1.09254843 · x · z` |
| `Y22` | `0.54627422 · (x² − y²)` |

**投影与卷积**：

```
L_lm = ∫ L(ω)·Y_lm(ω) dω                    // 逐样本：4π/N · Σ L·Y_lm
C_lm = A_l · L_lm                           // A0 = π，A1 = 2π/3，A2 = π/4
E(n) = Σ C_lm · Y_lm(n)                     // 辐照度，已含余弦卷积
```

- **折叠形式**：把 `A_l` 折进存储系数 `C_lm`，求值时不再乘 `A_l`；9 个系数 × RGB = 27 个 f32，存 9 行 `vec4`（xyz = RGB，w = 0），共 144B。
- **能量自检**：均匀天空辐亮度 `L` → 只有 `L00 = 4π·Y00·L` → `C00 = π·L00` → `E(n) = C00·Y00 = πL`。所以"均匀天空 = 旧环境项"的换算仍是 `πL`，与 9.1/L1 口径一致。
- **投影方式**：从源 cubemap 采 1024 个 Hammersley 方向（`textureSampleLevel(cube, sampler, dir, 0)`），单个 workgroup 归约；成本一次性，不随帧走。也可以按 texel 立体角积分，二选一，施工时钉明。
- **为什么不用 L1**：L1 的 4 个系数（12B）只够线性梯度；地平线带、太阳侧亮区会被压平。L2 的 108B 在这个工程里可以忽略，收益是方向性明显更稳。9.3 的 16×L1 是另一条路（分段 LUT），不是"更细的 SH"。

### 0.4 高光：split-sum（预滤波 cubemap + DFG LUT）

标准 split-sum 近似：

```
prefiltered = textureSampleLevel(env_cube, sampler, R, perceptualRoughness · (mip_count − 1))
dfg         = textureSampleLevel(dfg_lut, sampler, vec2(NoV, perceptualRoughness), 0)
specular    = prefiltered · (F0 · dfg.x + dfg.y)
```

- `R = reflect(-V, n)`；`NoV = clampNoV(saturate(dot(n, V)))`；`F0 = PixelParams.f0`。
- **粗糙度用 perceptualRoughness**：合成 pass 是 `matparam.b`，透明 pass 是 `surf.perceptual_roughness`；不要用 `PixelParams.roughness`——后者是 α = p²，直接喂 LUT/mip 会把粗糙面算得过锐。
- **预滤波**：GGX 重要性采样，逐 mip 降低样本数（例如 128/96/64/48/32/24/16/8）；每个 mip 的样本围绕 `R`，使用低差异序列 + 每像素旋转。源 cubemap 只读 mip 0，目标写 mip 链；用独立目标或逐 mip 视图 + 屏障避免读写 hazard。
- **DFG LUT**：128×128 RG16F（或 RGBA16F 复用格式），一次性 compute 生成；`x = A`、`y = B`。如果工期紧，可先用 Karis 的 `EnvBRDFApprox` 解析近似顶上，9.2.7 再决定是否换 LUT——但标准 IBL 的收尾形态是 LUT。
- **energyCompensation**：Filament 的 `1 + F0·(1/dfg.y − 1)` 补多重散射能量；8.2 取 1.0，`Filament BRDF参考` §6 把 DFG LUT 留给 IBL。本段是否同开作为 9.2.7 的裁决项，默认先不开（避免与 LUT 同时引入两个变量）。

### 0.5 注入公式与覆盖范围

```
// IBL 开
ibl_diffuse  = pixel.diffuseColor · Fd_Lambert() · E(n) · ibl_intensity
ibl_specular = prefiltered(R) · (pixel.f0 · dfg.x + dfg.y) · ibl_intensity
shaded       = light_sum + ibl_diffuse + ibl_specular

// IBL 关（步骤 8 原文，逐位不变）
shaded       = light_sum + albedo · ambient_color
```

| 路径 | IBL 开 | IBL 关 |
|---|---|---|
| 不透明（合成 pass） | `ibl_diffuse + ibl_specular` | `albedo · ambient_color` |
| 透明（forward） | `ibl_diffuse + ibl_specular` | `albedo · ambient_color` |
| 背景 | 9.1 天空（`sky_enabled`） | 清屏色 |
| 调试模式 | 无光照直出 | 同左 |

- `Fd_Lambert()` 复用 `shaders/common/brdf.wgsl`，与直接光漫反射同口径；不另写 `1/π`。
- `diffuseColor = albedo·(1−metallic)`：金属没有 IBL 漫反射，只有高光；介电体的高光由 `F0` 控制。
- **AO 暂不乘 IBL**：与旧环境项同口径；室内漏光/缝隙过亮作为已知局限记录，AO 接入留给 9.7/9.8 裁决。
- **透明层接 IBL**：这是环境基线，不是 GI；9.7 的 SCGI 仍然只作用于不透明。
- **pre-exposure**：天空/IBL 的辐亮度与直接光同域，合成 pass 末尾统一乘 `EXPOSURE`；IBL 内部不乘。
- **IBL 关的硬线**：开关分支里原样保留 `albedo * frame.ambient_color.rgb`，不做等价变形。

### 0.6 捕获与更新：脏标记 + 节流，事件驱动

- **输入**：`SkySettings`（天顶/地平线/地面色、强度、指数、太阳角半径/增益/光晕）+ 方向光方向/颜色/强度。把它们的组合哈希成 `sky_hash`。
- **触发**：`sky_hash` 变化，或距上次捕获超过最小间隔（建议 0.1s），或 BRP 强制。TOD 是慢变量，0.1s 的滞后不可见；静止时零成本。
- **位置**：在帧录制的 ② compute 前置段，按 `ibl_dirty` 决定是否 dispatch 捕获 → SH 投影 → 预滤波；composite 之前用 `COMPUTE_SHADER_WRITE → FRAGMENT_SHADER_READ` 屏障。非脏帧整段跳过。
- 捕获/投影/预滤波都是 compute pass；捕获完成后把 cubemap 转到 `SHADER_READ_ONLY`，非脏帧 composite 直接采样，不重复迁移。各自的 set 布局（frame UBO / 源 cube / 目标 storage view）在 9.2.1 钉明。
- **可见天空 vs IBL 的时间差**：可见天空每帧由 `sky_radiance` 解析求值，IBL 最多滞后一个更新间隔；TOD 连续变化时允许这个滞后，记录在案。
- **太阳分离**：捕获用 `include_sun=false`；可见天空用 `true`。这样 TOD 改太阳方向时，IBL 只跟"天空底色/光晕"的慢变化，太阳本体的高光始终由直接光即时给出。
- **不接 TOD 系统**：本段只提供 `SkySettings` 与更新策略；TOD 驱动（昼夜循环）留给后续/宿主。

### 0.7 资源与描述符：cubemap 不能走 bindless 2D 表

现状核实（2026-11-20）：

- `BindlessTextureTable` 的 set0 是 `binding_array<texture_2d<f32>>`，只收 2D 视图；`GpuImage` / `ImageSpec` 也只建 `TYPE_2D`、只收 RGBA8（`src/render/resident/assets/image_spec.rs`、`images.rs`）。
- composite 现有 set：0 = 帧 UBO、1 = G-buffer 采样、2 = 光源分簇、3 = 阴影；透明现有 set：0 = bindless、1 = 帧 UBO、2 = 实例/材质、3 = 光源分簇。

因此本段新增**独立的 IBL 资源与描述符集**，不硬塞进 set0：

| 资源 | 形态 | 用途 |
|---|---|---|
| 源环境 cubemap | `VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT`，256²×6，RGBA16F，mip 链 | 捕获输出；SH 投影与预滤波的输入 |
| 预滤波 cubemap | 同上，mip 链（可与源同图，逐 mip 写；或独立图） | 高光采样 |
| DFG LUT | 2D，128×128，RG16F/RGBA16F，1 mip | split-sum 的 A/B |
| SH buffer | storage buffer，9×vec4（144B） | SH-L2 系数 |

- 视图：采样用 `VK_IMAGE_VIEW_TYPE_CUBE`；捕获/预滤波写入用 `VK_IMAGE_VIEW_TYPE_2D_ARRAY`（6 层），每 mip 一个 2D array 视图（WGSL storage texture 不带 mip 参数）。
- 捕获优先用 compute + `texture_storage_2d_array`；若 naga 目标版本不支持该类型，退化为 6 次 `texture_storage_2d` dispatch + 每面 2D 视图（接口不变，施工记录里注明退路）。
- 新 IBL set（composite 与 transparent 都加，group 4）：
  - b0 `texture_cube<f32>` 预滤波环境
  - b1 `sampler`（linear + clamp，cube 与 LUT 共用）
  - b2 `texture_2d<f32>` DFG LUT
  - b3 `storage buffer` SH（fragment 读；compute 投影写）
  - 9.3 再决定 b4 放 GlobalProbe（16×RGBL1）还是复用 b3 + mode。
- 管线布局从 4 组变 5 组：先核对 `maxBoundDescriptorSets`（2060 远大于 5；低端设备也要留降级路径）。
- 源 cubemap 与预滤波 cubemap 如果同图：捕获写 mip 0，预滤波逐 mip 写 mip 1..N，读 mip 0；每个 mip 之间加屏障，禁止同 mip 读写。若实现太绕，直接两张图，内存 2×3 MiB 可接受。

### 0.8 调试与 BRP

- `ibl_set`：`{"enabled": bool, "intensity": f32}`，缺省字段不动；回显当前值。
- `ibl_stats`：`enabled`、`intensity`、`ibl_mode`（9.2 恒 0）、9 个 SH 系数（RGB）、最近捕获的 `sky_hash`/计数、`gpu_lighting_ms` 均值。
- 调试视图新增两档：
  - `mode 9 = IBL 漫反射贡献`：`ibl_diffuse * EXPOSURE` 直出；
  - `mode 10 = IBL 高光贡献`：`ibl_specular * EXPOSURE` 直出。
  - `present_tonemap.wgsl` 已有 `frame.mode != 0` 跳过 bloom/tonemap 的旁路，新档自动继承；高光截断属视图语义。
- `RenderMode::ALL` 从 9 项扩到 11 项；`label`/`from_name`/`as_u32`/`NAME_HINT` 四处同步。
- egui 面板本段不接（BRP 可用即够判定线）。

## 1. 任务拆分

| 任务 | 交付 | 落点 | 状态 |
|---|---|---|---|
| 9.2.1 环境 cubemap 捕获 | 新 `EnvironmentCube` 资源（cube 兼容、256²×6、RGBA16F、mip、cube/2D-array/每 mip 视图）；`sky_capture.wgsl` 从 `sky_radiance(dir, false)` 写 mip 0；脏标记 + 节流；② 段录制与屏障 | `src/render/passes/ibl/`（新）、`shaders/scene/ibl/sky_capture.wgsl`、`shaders/common/sky.wgsl` | ⬜ |
| 9.2.2 SH-L2 投影 | `sh_project.wgsl`：1024 Hammersley 方向、9 系数投影、`A_l` 折叠、workgroup 归约；写 9×vec4 storage buffer；能量自检 | `shaders/scene/ibl/sh_project.wgsl`、`src/render/passes/ibl/`、`shaders/common/ibl.wgsl` | ⬜ |
| 9.2.3 漫反射注入 | 帧 UBO 272→288（`ibl_enabled`/`ibl_intensity`/`ibl_mode`）；composite + transparent 环境项替换为 `diffuseColor/π·E(n)·intensity`；IBL 关逐位回退 | `frame_uniform_table.rs`、`gbuffer_composite.wgsl`、`transparent_forward.wgsl` | ⬜ |
| 9.2.4 高光预滤波与 DFG LUT | `prefilter.wgsl`（GGX mip 链）+ `brdf_lut.wgsl`（128²，一次性）；粗糙度→mip；逐 mip 屏障/视图 | `shaders/scene/ibl/prefilter.wgsl`、`brdf_lut.wgsl`、`src/render/passes/ibl/` | ⬜ |
| 9.2.5 高光注入 | split-sum：`prefiltered(R) · (F0·A + B)`；composite + transparent；太阳高光仍走直接光；`energyCompensation` 裁决 | `shaders/common/ibl.wgsl`、两个着色 pass | ⬜ |
| 9.2.6 描述符/管线/调试/BRP | IBL set（group 4，composite + transparent）；`ibl_set`/`ibl_stats`；mode 9/10；`RenderMode::ALL` 9→11 | `composite_pipeline.rs`、`transparent_pipeline.rs`、`src/remote/features/ibl.rs`（新）、`render_mode.rs` | ⬜ |
| 9.2.7 判定线与计量 | IBL 关基线、均匀天空自检、金属/透明/室内对照、`gpu_lighting_ms` A/B、回归矩阵、施工记录 | 记录文件随施工产出 | ⬜ |

### 任务间依赖

- 9.2.1 → 9.2.2 与 9.2.4 可并行（都读源 cubemap），但 9.2.1 必须先能稳定产出正确的面方向。
- 9.2.3 依赖 9.2.2 + 9.2.3 自己加的 UBO 字段；9.2.5 依赖 9.2.4。
- 9.2.6 在 shader 与描述符都稳定后一次接；9.2.7 最后跑，IBL 关基线必须在代码改动前抓。

## 2. 判定线（照八段拆解 §2 9.2 细化）

1. **全帧步骤 8 逐位一致（硬线）**：`sky_enabled=false` + `ibl_enabled=false` 与 9.2 开工前 HEAD 同机位截图逐像素一致（允许截图通道噪声，不接受结构差异）。
2. **IBL 隔离**：固定 `sky_enabled`（开或关），切 `ibl_enabled`——不透明/透明的几何像素只在 IBL 贡献处变化；`ibl_enabled=false` 时受光像素与 9.2 前逐位一致。
3. **均匀天空自检**：`sky_set` 把天顶/地平线/地面都设为 `L`、`sun_gain=0`、`intensity=1`，投影后应满足 `E(n)=πL`；`ibl_stats` 的 9 系数只有 `C00≈πL/Y00` 非零（其他≈0）；介电体表面与旧环境项视觉同形。
4. **方向性**：非均匀天空下，朝上/朝下/朝太阳侧的表面拿到不同辐照度；mode 9 视图可读；旋转太阳方向时亮侧随之旋转。
5. **高光**：金属/光滑面出现天空反射；粗糙度增大时反射变糊且不出现"硬边 mip 跳变"；太阳高光只由直接光产生，IBL 不出现第二个太阳。
6. **透明层**：`ibl_enabled` 切换时透明物体颜色变化；9.7 的 SCGI 开关（后续）不影响透明。
7. **捕获正确性**：cubemap 六面无接缝、方向正确（±X/±Y/±Z 取样与可见天空一致）；IBL 中不含太阳圆盘（对准太阳方向的低粗糙金属不应出现天空盘高光）。
8. **TOD/更新**：连续改天空参数时，IBL 在节流窗口内跟随；`ibl_stats` 的捕获计数增加；静止时不增加（零成本）。
9. **室内局限**：Sponza 等室内场景的"天花板漏光/缝隙过亮"读图记录，不修就写进局限。
10. **回归**：既有判定线零回退（8.2.5 五脚本）、库测试、零 VUID、`clippy` 全净、`fmt --check` 全绿、WM_CLOSE exit 0。

## 3. 测试方案

- **场景**：Sponza（室内漏光）、zero_day（室外天空/大背景）、DamagedHelmet 或金属球（高光与粗糙度梯度）。
- **启动**：`cargo run -p ash_renderer --release -- --scene zero_day/zero_day_measure_one.glb`；BRP 端口开发实例 15702。
- **BRP 配方**（`tools/brp.py raw`）：
  - IBL 关：`--method ash_renderer/ibl_set --params-json '{"enabled":false}'`
  - IBL 开：`--method ash_renderer/ibl_set --params-json '{"enabled":true,"intensity":1.0}'`
  - 均匀天空：`--method ash_renderer/sky_set --params-json '{"zenith":[800,800,800],"horizon":[800,800,800],"ground":[800,800,800],"intensity":1.0,"sun_gain":0.0}'`
  - 方向天空：`--method ash_renderer/sky_set --params-json '{"zenith":[80,160,400],"horizon":[600,500,400],"ground":[50,40,30],"intensity":1.0}'`
  - 读数：`--method ash_renderer/ibl_stats` / `--method ash_renderer/perf_stats`
- **视觉验证**：PrintWindow 抓图 + 当前会话模型读图（先写期望画面描述再读图）；取证落 `.temp/9.2/`，结论与截图路径入施工记录。
- **数值与构建**：`cargo fmt --check -p ash_renderer`、`cargo clippy -p ash_renderer --all-targets`、`cargo test -p ash_renderer`；运行日志抓 VUID 与 `WM_CLOSE` 退出码。
- **回归脚本**：`tools/hiz_test.py`、`tools/scene_lod.py`、`tools/scene_stream.py`、`tools/scene_fine.py`、`tools/scene_sponza.py`（按 8.2.5 总验收口径）。

## 4. 风险与坑

1. **立方体面方向约定**：Vulkan 的 cube face 顺序与 +X/−X/+Y/−Y/+Z/−Z 的 UV 方向容易写反；先用 6 个轴向方向取样对照可见天空，再谈 SH/预滤波。面接缝是方向映射错误的第一信号。
2. **`GpuImage` 只有 2D RGBA8**：cubemap 不能走资产上传链；必须新建内部资源（`CUBE_COMPATIBLE`、RGBA16F、mip、cube/2D-array 视图）。`ImageSpec` 的格式/维度拒绝是显式的，别硬改它。
3. **storage texture 的 mip/array 支持**：WGSL storage texture 不带 mip 参数，需要每 mip 视图；`texture_storage_2d_array` 若 naga 目标不支持，退化为 6 次 2D dispatch + 面视图。
4. **SH 投影归一化**：`4π/N` 与 `A_l` 折叠只做一次；`C00` 的 `4π²Y00² = π` 自检必须过。投影样本数不足会让 `Y22` 等高阶项抖动，1024 起，必要时 2048。
5. **预滤波成本**：逐 mip 的样本数、mip 视图与屏障是主要工程量；脏标记/节流必须真的生效，不能每帧重烘全部 mip。
6. **DFG LUT 格式与采样**：LUT 是线性数据，不能走 sRGB；`RG16F` 要查 `FORMAT_R16G16_SFLOAT` 的采样/过滤支持；不支持就用 `RGBA16F`。
7. **描述符集数量/布局**：composite 与 transparent 各加第 5 组；管线布局、描述符池、set 绑定必须同步。低端设备要核对 `maxBoundDescriptorSets`。
8. **pre-exposure 双乘**：天空/IBL 都在 pre-exposure 域，合成末尾乘一次 `EXPOSURE`；不要在 SH/预滤波里再乘。
9. **粗糙度口径**：`PixelParams.roughness` 是 α = p²；LUT 与 mip 选择必须用 perceptual roughness（`matparam.b` / `surf.perceptual_roughness`）。
10. **太阳双计**：捕获 `include_sun=false`；可见天空 `true`。如果发现金属上出现两个太阳高光，先查这条。
11. **室内漏光**：IBL 没有天空遮挡；Sponza 会明显偏亮。不修就记录，不要用降低 `ibl_intensity` 掩盖（会把室外也压暗）。
12. **UBO 扩列**：`FRAME_UBO_SIZE` 272→288，`FrameUniformTableLayout` 断言、`pack_frame_uniforms`、`frame.wgsl` 头注、`gbuffer_write.wgsl` 注释同改。
13. **`RenderMode::ALL` 长度**：`[Self; 9]` → `[Self; 11]`，`label`/`from_name`/`as_u32`/`NAME_HINT` 四处同步；单元测试往返断言会覆盖新档。
14. **BRP 参数校验**：`intensity ≥ 0`、有限值；错误回 `BrpError` 不 panic。
15. **运行实例锁 exe**：运行中的旧实例会让 `cargo build` 链接失败（os error 5）；改动前先 WM_CLOSE，确认退出码为 0。

## 5. 边界

- **不产生**：世界空间缓存、cell/patch、Volume、射线、BLAS/TLAS、motion vectors、屏幕 gather、SCGI 开关（9.4～9.8）。
- **不做**：SSR、局部反射探针、AO 本体、烘焙/lightmap、云层/大气散射、自动曝光/pre-exposure 补偿、多相机、egui 面板。
- **不主张**：镜面间接（只做天空环境高光）、方向性遮蔽、室内天空遮挡、IBL 的多重散射补偿（9.2.7 裁决，默认不开）。
- **不改**：阴影链、后处理链、G-buffer 布局、GPU-driven 闭环、光源常驻表。
- **透明层**：本段接 IBL，但不接 SCGI；9.7 仍只改不透明合成。

## 6. 材料

- [步骤 9 README](../README.md)——步骤索引、八段顺序与开工前置。
- [八段拆解：天空盒、IBL 与表面缓存全局光照](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§1 选型、§2 9.2 节（交付/判定线草案）、§5 目录组织。
- [Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)——§4 直接光 BRDF 与 `energyCompensation`、§6 的 DFG LUT 归属、§6 第 6 条预曝光口径。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——环境 cubemap 捕获、太阳圆盘排除、`GlobalProbe` 更新条件（9.3 的依据）。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——探针的 5D 光照场、L1/L2、16 桶与 L2 的分工。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——`DistanceFallback` 与全局探针的定位（9.3/9.7 的边界）。
- 本工程现状锚点：`shaders/scene/gbuffer/gbuffer_composite.wgsl`（环境项与调试模式）、`shaders/scene/transparent/transparent_forward.wgsl`（透明环境项）、`shaders/common/frame.wgsl`（UBO 272B，9.1 后）、`src/render/resident/tables/frame_uniform_table.rs`（布局镜像与断言）、`src/render/passes/lighting/composite_pipeline.rs` 与 `src/render/passes/transparent/transparent_pipeline.rs`（现有 set 数量）、`src/render/resident/tables/bindless_texture_table.rs`（set0 只收 2D）。
