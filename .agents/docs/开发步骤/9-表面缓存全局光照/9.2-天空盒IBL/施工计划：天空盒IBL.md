# 施工计划：9.2 天空盒 IBL

> 2026-11-20 立案（纯文档，重规划）；**2026-11-23 按 9.1 物理天空落地同步修订**（渐变天空已被物理天空替换，本段捕获源、UBO 尺寸、自检口径随之更新，见 §0.0）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.2](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.2 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：📝 施工计划已立案，未开工（2026-11-20 立案；2026-11-23 同步修订）**。

**一句话说明**：把 9.1 的物理天空捕获为环境 cubemap，投影 SH-L2 漫反射并预滤波高光，替换不透明与透明的常量环境项；本段不产生世界空间缓存、射线或 SCGI。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-23 同步修订清单（相对 2026-11-20 版）

| 项 | 旧口径 | 现行口径 | 依据 |
|---|---|---|---|
| 捕获源 | 解析渐变 `sky_radiance(dir, false)` | **物理天空**（三介质单次散射 + Chapman 透射率 + 地面反照率 + 太阳圆盘） | 9.1 追加段同日收官，[9.1.6](../9.1-程序化天空盒/9.1.6-物理天空模型与参数面.md) |
| 太阳排除 | 「排除太阳」 | 只排除**圆盘**；米氏前向峰的宽亮区仍进捕获（HDRP cubemap 烘焙同口径） | `shaders/common/sky.wgsl` 头注 + 9.1 追加计划坑 4 |
| 帧 UBO | 272→288B | **304→336B**（9.1 已到 304；本段加 `ibl_*` 12B + 4B 对齐垫 + `sky_uniform` 16B = +32B） | `frame_uniform_table.rs:39` 实测 304 |
| 均匀天空自检 | `sky_set` 设天顶/地平线/地面同色 | 物理天空无「天顶/地平线/地面色」，改由 **9.2.0 `sky_uniform` 常量天空开关**提供；`sky_set` 只剩大气参数 | `src/remote/features/sky.rs:96-99` 参数面已换新 |
| 捕获高度 | 未提 | `sky_radiance` 以 `frame.camera_pos.y` 为海拔；捕获高度入 hash（按 100 m 量化） | `sky.wgsl` 相机海拔假设（9.1 追加计划坑 6） |
| storage 2D array | 「naga 目标版本可能不支持，需退化」 | **已核支持**：naga 30.0.1 `texture_storage_2d_array`（`front/wgsl/parse/conv.rs:552`），build.rs 反射为 `DescKind::StorageImage`；6 次 2D dispatch 只留作退路 | 本机 checkout 核实 |
| 阻挡屏障 | 未提 | `ImageBarrier` 硬编码 mip0/layer0（`core/record/barrier.rs:151-152`），cube 需要全子资源范围，本段要扩 | 本机 checkout 核实 |

### 0.1 两层语义：IBL 是新的环境基线，SCGI 只替换不透明漫反射

从本段起，场景的环境光分两层：

```
源：9.1 天空 → 9.2 环境 cubemap（天空变化时重烘）
  ├── 高光：GGX 预滤波 mip + DFG LUT        —— 永远走 IBL，9.3 明确"不改高光路径"
  └── 漫反射：SH-L2（ibl_mode=0，默认候选）
              或 16 桶 RGBL1（9.3，ibl_mode=1，与 patch 同格式）
IBL 基线（9.2～9.3）：所有受光材质（不透明 + 透明）的环境项；无位置、无遮挡
SCGI（9.7）：只替换"不透明像素、在 Volume 内"的漫反射间接项；
            体积外保持 9.3 的漫反射编码——它仍属 IBL 基线，不是另一层
```

- **9.3 的 GlobalProbe 不是 IBL 的替代层**：它与 SH-L2 同源（都从这张天空 cubemap 投影）、同层（都无位置、无遮挡），换的只是漫反射编码；高光、透明层、体积外漫反射仍全部由 IBL 提供。是否成为默认由 9.3 的 A/B 决定，不预设"取代"。
- `ibl_enabled = false` 时，不透明与透明都回到步骤 8 的 `albedo * ambient_color`，逐位一致；这是回归硬线。
- `ibl_enabled = true` 时，环境项被 IBL 的漫反射 + 高光**替换**（不是叠加）；透明层也接 IBL，因为它是新的环境基线，不是 GI。
- SCGI 的 `gi_enabled` 到 9.7 才出现；它只影响不透明合成，不影响透明层（透明像素不进不透明 G-buffer，没有播种足迹也没有屏幕查表位置）。
- 背景天空归 9.1 的 `sky_enabled`，与 `ibl_enabled` 正交：可以"有天空无 IBL"（对照），也可以"有 IBL 无可见天空"（只测材质）。
- `ibl_enabled` 段内默认关（BRP 开），避免 9.2 一落地就改变所有场景的基线；9.8 验收后再拍板是否默认开。

### 0.2 数据链：一张源 cubemap，三个派生结果

```
sky_radiance(dir, include_sun=false)          // 9.1 物理天空；捕获只排除太阳圆盘
        │
        ├── 环境 cubemap（256²×6 RGBA16F，mip 链，源）
        │        ├── SH-L2 投影 → 9×RGB 系数 → storage buffer   （漫反射）
        │        └── GGX 预滤波 → mip 1..8（mip 0 保留锐利源）  （高光）
        └── DFG/BRDF LUT（128²，一次性生成）                    （高光）
```

- **捕获排除太阳圆盘**：太阳是小立体角高频亮斑；进 SH 会污染低频项，进预滤波会和直接光的高光双计。可见天空仍画圆盘（9.1 的 `include_sun=true`）。
- **米氏前向峰留在捕获里**：物理天空的太阳附近亮区是散射，不是圆盘；HDRP 的 cubemap 烘焙同样保留它。它会让低粗糙金属在太阳方向出现**宽亮区**（不是第二个圆盘），判定线按此写。
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
- **数值锚点**：均匀辐亮度 `L` → 只有 l=0 非零 → `E(n) = πL`。这个等价关系仍是"均匀天空 = 旧环境项"的换算依据（9.2.0 的 `sky_uniform` 用它验收），但它**不再是物理天空可达的参数组合**。
- **自检两条（不依赖均匀天空）**：见 §0.6 末。
- **为什么不用 L1**：L1 的 4 个系数（12B）只够线性梯度；地平线带、太阳侧亮区会被压平。L2 的 108B 在这个工程里可以忽略，收益是方向性明显更稳。9.3 的 16×L1 是另一条路（分段 LUT），不是"更细的 SH"。

### 0.4 高光：split-sum（预滤波 cubemap + DFG LUT）

标准 split-sum 近似：

```
prefiltered = textureSampleLevel(env_cube, sampler, R, perceptualRoughness · (mip_count − 1))
dfg         = textureSampleLevel(dfg_lut, sampler, vec2(NoV, perceptualRoughness), 0)
specular    = prefiltered · (F0 · dfg.x + dfg.y)
```

- `R = reflect(-V, n)`；`NoV = clampNoV(saturate(dot(n, V)))`；`F0 = pixel.f0`。
- **粗糙度用 perceptualRoughness**：合成 pass 是 `matparam.b`，透明 pass 是 `surf.perceptual_roughness`；不要用 `PixelParams.roughness`——后者是 α = p²，直接喂 LUT/mip 会把粗糙面算得过锐。
- **预滤波**：GGX 重要性采样，逐 mip 降低样本数（例如 128/96/64/48/32/24/16/8）；每个 mip 的样本围绕 `R`，使用低差异序列 + 每像素旋转。源 cubemap 只读 mip 0，目标写 mip 1..8；mip 0 保留锐利源（roughness=0 的镜面反射就是它）。
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

- **输入**：`SkySettings` 的物理参数（`intensity`、`planet_radius`、`atmosphere_height`、三介质散射/吸收系数、两个标高、`mie_anisotropy`、`ozone_center`/`ozone_width`、`ground_albedo`、`sun_gain`/`sun_radius`）+ **`sky_uniform`（开关 + 颜色，否则切常量天空不会触发重烘）** + 方向光方向/颜色/强度 + **相机海拔量化值**（`floor(camera_pos.y / 100 m)`）。把它们的 f32 位模式哈希成 `sky_hash`。
- **相机海拔入 hash 的理由**：物理天空用 `frame.camera_pos.y` 当海拔（9.1 追加计划坑 6），地面行走的米级差可忽略（标高 8000 m），但飞行/升降台会改变天际线；按 100 m 量化后，地面漫游永不触发重烘，飞行时按需重烘。
- **触发**：`ibl_enabled` 为真、且（`captured_hash` 缺失/不等 或 DFG LUT 未生成 或 距上次捕获超过最小间隔 0.1s）。`ibl_enabled=false` 时整段不录、不 dispatch（零成本）；打开开关的同一帧在 ② 段完成捕获，合成（⑤ 段）即可用，不存在滞后一帧。
- **位置**：帧录制的 ② compute 前置段（`render/frame/record.rs:154` 之后、shadow 之前），按 `ibl_dirty` 决定是否 dispatch 捕获 → SH 投影 → 预滤波；非脏帧整段跳过，cubemap 保持 `SHADER_READ_ONLY`，不录任何屏障。
- **屏障**：捕获/预滤波写 cubemap 用 `GENERAL` 布局；**捕获写 mip0 → 预滤波读 mip0** 之间要一条 compute→compute 的 `SHADER_WRITE → SHADER_READ`（同子资源 RAW）；预滤波写 mip1..8 与 mip0 读是不同子资源，不互等；整段写完后一条**全子资源**屏障（`level_count = 9`、`layer_count = 6`）转 `SHADER_READ_ONLY_OPTIMAL`，`COMPUTE_SHADER_WRITE → FRAGMENT_SHADER_READ`；SH/stats storage buffer 与 DFG LUT 同批用 `MemoryBarrierDesc` 做 compute 写 → 片元读。现有 `ImageBarrier` 只覆盖 mip0/layer0（`core/record/barrier.rs:151-152`），本段需给结构体加子资源范围字段（默认值保持 mip0/layer0，`..` 更新式调用不受影响）。
- **可见天空 vs IBL 的时间差**：可见天空每帧由 `sky_radiance` 解析求值，IBL 最多滞后一个更新间隔；TOD 连续变化时允许这个滞后，记录在案。
- **太阳分离**：捕获用 `include_sun=false`；可见天空用 `true`。这样 TOD 改太阳方向时，IBL 只跟"天空底色/光晕"的慢变化，太阳本体的高光始终由直接光即时给出。
- **不接 TOD 系统**：本段只提供 `SkySettings` 与更新策略；TOD 驱动（昼夜循环）留给后续/宿主。
- **SH 自检两条**（写进 9.2.2，结果由 `ibl_stats` 回显，不依赖均匀天空）：
  1. **l=0 归一化**：同一组投影方向同时算 `mean(L)`，校验 `C00 == π·4π·Y00·mean`（等价 `C00·Y00 == π·mean`）；
  2. **求值对积分**：取 6 轴向 + 8 对角共 14 个法线，比较 `E_sh(n)` 与同一方向集上的暴力余弦积分 `E_ref(n) = (4π/N)·Σ L·max(n·ω,0)`，报最大相对误差。
  - `sky_uniform`（9.2.0）打开时，两条自检同时给出"均匀天空 = πL"的解析锚点。

### 0.7 资源与描述符：cubemap 不能走 bindless 2D 表

现状核实（2026-11-23）：

- `BindlessTextureTable` 的 set0 是 `binding_array<texture_2d<f32>>`，只收 2D 视图；`ImageSpec` 只收 2D、RGBA8（`src/render/resident/assets/image_spec.rs:51-58`）。
- 全工程**没有任何 cubemap**（无 `CUBE_COMPATIBLE`/`TYPE_CUBE`/`texture_cube`）——本段是第一次引入，视图/布局/格式查询都要新建。
- composite 现有 set：0 = 帧 UBO、1 = G-buffer 采样、2 = 光源分簇、3 = 阴影（`composite_pipeline.rs:20-25`）；透明现有 set：0 = bindless、1 = 帧 UBO、2 = 实例/材质、3 = 光源分簇（`transparent_pipeline.rs:23-28`）。
- 存储图像链路**已验证可用**：`DescKind::StorageImage` 存在（`set_layout.rs:17`），hi-z 已用 `texture_storage_2d<r32float, write>`（`shaders/gpu_driven/hiz_downsample.wgsl:17`）；naga 30.0.1 的 WGSL 前端支持 `texture_storage_2d_array`（`front/wgsl/parse/conv.rs:552`），build.rs 的反射把 `ImageClass::Storage` 映射为 `StorageImage`。

因此本段新增**独立的 IBL 资源与描述符集**，不硬塞进 set0：

| 资源 | 形态 | 用途 |
|---|---|---|
| 源环境 cubemap | `VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT`，256²×6，RGBA16F，9 级 mip | 捕获输出（mip0）；SH 投影与预滤波的输入 |
| 预滤波 cubemap | 同上（可与源同图，mip 1..8 覆盖写；或独立图） | 高光采样 |
| DFG LUT | 2D，128×128，RG16F/RGBA16F，1 级 mip | split-sum 的 A/B |
| SH buffer | `BufferRole::Storage`（宿主可见），9×vec4（144B） | SH-L2 系数（compute 写、fragment 读、BRP 读） |
| 自检 stats buffer | `BufferRole::Storage`，两条自检的最大误差 + 捕获计数 | `ibl_stats` 回显 |

- **格式能力查询**（照 hi-z 的 `hiz.rs:293` 同款）：`R16G16B16A16_SFLOAT` 的 optimal 特性需含 `STORAGE_IMAGE | SAMPLED_IMAGE | SAMPLED_IMAGE_FILTER_LINEAR`；`R16G16_SFLOAT`（DFG LUT）另查 `SAMPLED_IMAGE_FILTER_LINEAR`。缺 `STORAGE_IMAGE` 时初始化直接报错（不静默降级）。
- 视图：采样用 `VK_IMAGE_VIEW_TYPE_CUBE`；捕获/预滤波写入用 `VK_IMAGE_VIEW_TYPE_2D_ARRAY`（6 层），**每 mip 一个视图**（WGSL storage texture 不带 mip 参数）。
- 捕获用 compute + `texture_storage_2d_array`（mip0，一次 dispatch 写 6 面）；6 次 `texture_storage_2d` + 每面 2D 视图只作退路，施工记录里注明是否用到。
- 新 IBL set（composite 与 transparent 都加，group 4）：
  - b0 `texture_cube<f32>` 预滤波环境
  - b1 `sampler`（linear + clamp，cube 与 LUT 共用）
  - b2 `texture_2d<f32>` DFG LUT
  - b3 `storage buffer` SH（fragment 读；compute 投影用独立管线声明 `read_write`，两者不共存于同一模块）
  - b4 留空（9.3 再定放 GlobalProbe 还是复用 b3 + mode）
- 管线布局从 4 组变 5 组；本机 `maxBoundDescriptorSets` 未查（2060 应远大于 5），9.2.6 建管线时一并读设备属性记录。
- 源 cubemap 与预滤波 cubemap 同图的写法：捕获写 mip0，预滤波逐 mip 写 1..8、只读 mip0（不同子资源，无同 mip 读写冲突）；整段保持 `GENERAL`，写完一次性转 `SHADER_READ_ONLY`。若实现太绕，直接两张图（内存 2×3 MiB 可接受）。

### 0.8 调试与 BRP

- `ibl_set`：`{"enabled": bool, "intensity": f32}`，缺省字段不动；回显当前值。
- `ibl_stats`：`enabled`、`intensity`、`ibl_mode`（9.2 恒 0）、9 个 SH 系数（RGB）、两条自检误差、最近捕获的 `sky_hash`/捕获计数/捕获耗时、`gpu_lighting_ms` 均值。读数允许滞后一帧（BRP 不是正确性路径，与透明 `counts` 同口径）。
- 调试视图新增两档：
  - `mode 9 = IBL 漫反射贡献`：`ibl_diffuse * EXPOSURE` 直出；
  - `mode 10 = IBL 高光贡献`：`ibl_specular * EXPOSURE` 直出。
  - `present_tonemap.wgsl:220` 已有 `frame.mode != 0u` 跳过 bloom/tonemap 的旁路，新档自动继承；高光截断属视图语义。
- `RenderMode::ALL` 从 9 项扩到 11 项（`render_mode.rs:36`）；`label`/`from_name`/`as_u32`/`NAME_HINT` 四处同步，名称提案 `ibl_diffuse` / `ibl_specular`。
- egui 面板本段不接（BRP 可用即够判定线）；9.1 的天空面板已存在，IBL 页可后置。

### 0.9 常量天空开关（9.2.0）：只为自检，不进生产路径

- 帧 UBO 加 `sky_uniform: vec4f`（xyz = 辐亮度，w = 1 开 / 0 关）；`sky_radiance` 开头一句：`if (frame.sky_uniform.w > 0.5) { return frame.sky_uniform.rgb; }`。
- 打开时可见天空与捕获同时变成常量 `L`（`include_sun` 被忽略），于是 `E(n) = πL` 是解析真值；这正是 9.2/9.3 判定线要的锚点。
- 生产路径默认 `w = 0`，物理天空行为逐位不变；开关只由 BRP `sky_uniform` 方法写（独立方法：清空语义与 `sky_set` 的"缺省字段不动"冲突）。
- 代价：UBO 多 16B、`sky.wgsl` 一个分支；收益是 9.2 与 9.3 都恢复"均匀天空"这个最强的能量自检。施工可推翻（退回 §0.6 的两条积分自检），但须在施工记录说明。

## 1. 任务拆分

| 任务 | 交付 | 落点 | 状态 |
|---|---|---|---|
| 9.2.0 常量天空开关与 UBO 扩列 | 帧 UBO **304→336**：`ibl_enabled`(304)/`ibl_mode`(308)/`ibl_intensity`(312)/pad + `sky_uniform`(320..336)；`sky_radiance` 常量分支；`SkySettings` 加 `uniform_enabled`/`uniform_radiance`；BRP `sky_uniform`；布局断言、`pack_frame_uniforms`、`identity_frame_uniforms`、`write_frame_ubo`、`frame.wgsl` 头注同步 | `frame_uniform_table.rs`、`frame.wgsl`、`sky.wgsl`、`passes/sky/mod.rs`、`remote/features/sky.rs`、`driver/host/frame/scene.rs` | ⬜ |
| 9.2.1 环境 cubemap 捕获 | `EnvironmentCube` 资源（cube 兼容、256²×6、RGBA16F、9 mip；cube/2D-array/每 mip 视图；格式能力查询）；`sky_capture.wgsl` 从 `sky_radiance(dir, false)` 写 mip0；`sky_hash` + 0.1s 节流 + `ibl_enabled` 门控；② 段录制与全子资源屏障 | `src/render/passes/ibl/`（新）、`shaders/scene/ibl/sky_capture.wgsl`、`shaders/common/ibl.wgsl`、`core/record/barrier.rs`（子资源范围） | ⬜ |
| 9.2.2 SH-L2 投影 | `sh_project.wgsl`：1024 Hammersley 方向、9 系数投影、`A_l` 折叠、workgroup 归约；写 9×vec4 storage buffer；两条自检写 stats buffer | `shaders/scene/ibl/sh_project.wgsl`、`src/render/passes/ibl/`、`shaders/common/ibl.wgsl` | ⬜ |
| 9.2.3 漫反射注入 | composite + transparent 环境项替换为 `diffuseColor/π·E(n)·intensity`；IBL set 建集并绑进两条管线；IBL 关逐位回退 | `gbuffer_composite.wgsl`、`transparent_forward.wgsl`、`composite_pipeline.rs`、`transparent_pipeline.rs`、`driver/host/init.rs`（布局登记）、`render/frame/record.rs`（⑤ 段绑定） | ⬜ |
| 9.2.4 高光预滤波与 DFG LUT | `prefilter.wgsl`（GGX mip 1..8）+ `brdf_lut.wgsl`（128²，一次性）；粗糙度→mip；mip 视图 | `shaders/scene/ibl/prefilter.wgsl`、`brdf_lut.wgsl`、`src/render/passes/ibl/` | ⬜ |
| 9.2.5 高光注入 | split-sum：`prefiltered(R) · (F0·A + B)`；composite + transparent；太阳高光仍走直接光；`energyCompensation` 裁决 | `shaders/common/ibl.wgsl`、两个着色 pass | ⬜ |
| 9.2.6 调试/BRP/管线收口 | `ibl_set`/`ibl_stats`（新 `src/remote/features/ibl.rs` + `remote/mod.rs` 登记）；mode 9/10；`RenderMode::ALL` 9→11；`RenderFeatures` 新增 IBL 束字段 | `src/remote/features/ibl.rs`、`render/frame/render_features.rs`、`render_mode.rs` | ⬜ |
| 9.2.7 判定线与计量 | IBL 关基线、`sky_uniform` 均匀天空自检、太阳排除负例、金属/透明/室内对照、捕获耗时、回归矩阵、施工记录 | 记录文件随施工产出 | ⬜ |

### 任务间依赖

- 9.2.0 先落（UBO 字段与常量天空是后面所有验收的锚点）；9.2.1 → 9.2.2 与 9.2.4 可并行（都读源 cubemap），但 9.2.1 必须先能稳定产出正确的面方向。
- 9.2.3 依赖 9.2.2 **与 IBL 描述符集**，所以 set 建集与两条管线绑定跟随 9.2.3，不留到 9.2.6；9.2.5 依赖 9.2.4。
- 9.2.6 只剩调试档与 BRP；9.2.7 最后跑，IBL 关基线必须在代码改动前抓。

## 2. 判定线（照八段拆解 §2 9.2 细化）

1. **全帧步骤 8 逐位一致（硬线）**：`sky_enabled=false` + `ibl_enabled=false` 与 9.2 开工前 HEAD 同机位截图逐像素一致（允许截图通道噪声，不接受结构差异）。
2. **9.1 基线不回退**：`sky_enabled=true` + `ibl_enabled=false` 与 9.1 收官版逐位一致（背景与受光像素都不变）。
3. **IBL 隔离**：固定 `sky_enabled`（开或关），切 `ibl_enabled`——不透明/透明的几何像素只在 IBL 贡献处变化；背景不变。
4. **常量天空自检**：`sky_uniform` 设 `L`、`ibl_intensity=1`，投影后 `ibl_stats` 应满足 `E(n)=πL`（6 轴向误差 <1%），9 系数只有 l=0 非零；介电体表面与旧环境项视觉同形；两条积分自检误差 <1%。
5. **方向性**：非均匀物理天空下，朝上/朝下/朝太阳侧的表面拿到不同辐照度；mode 9 视图可读；旋转太阳方向时亮侧随之旋转。
6. **高光**：金属/光滑面出现天空反射；粗糙度增大时反射变糊且不出现"硬边 mip 跳变"；**太阳圆盘只由直接光产生**——对准太阳的低粗糙金属不得出现第二个月牙/圆盘（米氏前向峰的宽亮区允许，见 §0.2）。
7. **太阳排除负例**：`rayleigh=0`、`mie=0`、`ozone=0`、`ground_albedo=0`、`sun_gain>0` 时，背景只剩太阳圆盘；此配置下 `ibl_enabled=true` 的 mode 9/10 应几乎全黑（证明圆盘未进捕获）。
8. **透明层**：`ibl_enabled` 切换时透明物体颜色变化；9.7 的 SCGI 开关（后续）不影响透明。
9. **捕获正确性**：cubemap 六面无接缝、方向正确（±X/±Y/±Z 取样与可见天空一致）。
10. **TOD/更新**：连续改天空参数时，IBL 在节流窗口内跟随；`ibl_stats` 的捕获计数增加；静止时不增加；`ibl_enabled=false` 时捕获计数不增加。
11. **室内局限**：Sponza 等室内场景的"天花板漏光/缝隙过亮"读图记录，不修就写进局限。
12. **回归**：既有判定线零回退（8.2.5 五脚本）、库测试、零 VUID、`clippy` 全净、`fmt --check` 全绿、WM_CLOSE exit 0。

## 3. 测试方案

- **场景**：Sponza（室内漏光）、zero_day（室外天空/大背景）、DamagedHelmet 或金属球（高光与粗糙度梯度）。
- **启动**：`cargo run -p ash_renderer --release -- --scene zero_day/zero_day_measure_one.glb`；BRP 端口开发实例 15702。
- **BRP 配方**（`tools/brp.py raw`）：
  - IBL 关：`--method ash_renderer/ibl_set --params-json '{"enabled":false}'`
  - IBL 开：`--method ash_renderer/ibl_set --params-json '{"enabled":true,"intensity":1.0}'`
  - 常量天空：`--method ash_renderer/sky_uniform --params-json '{"enabled":true,"color":[800,800,800]}'`；关闭 `{"enabled":false}`
  - 太阳排除负例：`--method ash_renderer/sky_set --params-json '{"rayleigh":[0,0,0],"mie":0,"ozone":[0,0,0],"ground_albedo":[0,0,0],"sun_gain":3.0}'`
  - 物理天空定向对照：`--method ash_renderer/sky_set --params-json '{"mie":20e-6,"sun_gain":2.0}'`（只改米氏，观察太阳侧宽亮区）
  - 读数：`--method ash_renderer/ibl_stats` / `--method ash_renderer/perf_stats`
- **视觉验证**：PrintWindow 抓图 + 当前会话模型读图（先写期望画面描述再读图）；取证落 `.temp/9.2/`，结论与截图路径入施工记录。
- **数值与构建**：`cargo fmt --check -p ash_renderer`、`cargo clippy -p ash_renderer --all-targets`、`cargo test -p ash_renderer`；运行日志抓 VUID 与 `WM_CLOSE` 退出码。
- **回归脚本**：`tools/hiz_test.py`、`tools/scene_lod.py`、`tools/scene_stream.py`、`tools/scene_fine.py`、`tools/scene_sponza.py`（按 8.2.5 总验收口径）。

## 4. 风险与坑

1. **立方体面方向约定**：Vulkan 的 cube face 顺序与 +X/−X/+Y/−Y/+Z/−Z 的 UV 方向容易写反；先用 6 个轴向方向取样对照可见天空，再谈 SH/预滤波。面接缝是方向映射错误的第一信号。
2. **`ImageSpec`/`GpuImage` 只有 2D RGBA8**：cubemap 不能走资产上传链；必须新建内部资源（`CUBE_COMPATIBLE`、RGBA16F、mip、cube/2D-array 视图）。`ImageSpec` 的格式/维度拒绝是显式的，别硬改它。
3. **`ImageBarrier` 只覆盖 mip0/layer0**（`barrier.rs:151-152`）：cube 的布局转换必须覆盖 9 级 mip × 6 层，否则其余子资源布局未定义（验证层会抓）。给结构体加子资源范围字段，默认保持旧行为。
4. **storage texture 的 array 形状**：naga 30.0.1 支持 `texture_storage_2d_array`（已核），但描述符写入是每 mip 一张 2D-array 视图（6 层）；`DescKind::StorageImage` 只校验种类、不校验维度，写错视图类型由验证层兜。6 次 2D dispatch 只作退路。
5. **SH 投影归一化**：`4π/N` 与 `A_l` 折叠只做一次；`C00` 的 l=0 归一化自检必须过（§0.6），投影样本数不足会让 `Y22` 等高阶项抖动，1024 起，必要时 2048。
6. **预滤波成本**：逐 mip 的样本数、mip 视图与屏障是主要工程量；脏标记/节流必须真的生效，不能每帧重烘全部 mip。源 cubemap 与预滤波同图时，整段保持 `GENERAL` 再一次性转采样布局。
7. **DFG LUT 格式与采样**：LUT 是线性数据，不能走 sRGB；`RG16F` 要查 `FORMAT_R16G16_SFLOAT` 的线性过滤支持；不支持就用 `RGBA16F`。
8. **描述符集数量/布局**：composite 与 transparent 各加第 5 组；管线布局、描述符池、set 绑定、`driver/host/init.rs` 的 `SetLayouts` 登记必须同步；本机 `maxBoundDescriptorSets` 未查，建管线时读属性记录。
9. **pre-exposure 双乘**：天空/IBL 都在 pre-exposure 域，合成末尾乘一次 `EXPOSURE`；不要在 SH/预滤波里再乘。
10. **粗糙度口径**：`PixelParams.roughness` 是 α = p²；LUT 与 mip 选择必须用 perceptual roughness（`matparam.b` / `surf.perceptual_roughness`）。
11. **太阳双计**：捕获 `include_sun=false`；可见天空 `true`。米氏前向峰会留在捕获里，金属上允许出现宽亮区；出现**第二个圆盘**才说明 `include_sun` 没生效。
12. **室内漏光**：IBL 没有天空遮挡；Sponza 会明显偏亮。不修就记录，不要用降低 `ibl_intensity` 掩盖（会把室外也压暗）。
13. **UBO 扩列**：`FRAME_UBO_SIZE` 304→336，`FrameUniformTableLayout` 断言（新增 304/308/312/320 与 size）、`pack_frame_uniforms`、`identity_frame_uniforms`、`frame.wgsl` 头注偏移列表、`write_frame_ubo` 五处同改。
14. **`RenderMode::ALL` 长度**：`[Self; 9]` → `[Self; 11]`，`label`/`from_name`/`as_u32`/`NAME_HINT` 四处同步；单元测试往返断言会覆盖新档。
15. **BRP 参数校验**：`intensity ≥ 0`、有限值；错误回 `BrpError` 不 panic。
16. **捕获成本**：物理天空是 16 步积分 × 256²×6（约 39 万像素），比渐变天空贵一个量级；一次捕获必须在 9.2.1 用时间戳记读数（全帧差或单独 timestamp），节流不生效就会被逐帧重烘。
17. **捕获高度**：`sky_radiance` 以 `frame.camera_pos.y` 为海拔，捕获 cube 与相机高度绑定；按 100 m 量化入 hash。室内/室外米级差可忽略，但飞行场景会让 IBL 滞后一段高度，记录在案。
18. **`sky_uniform` 只作测试**：打开时可见天空也是常量，不要拿它当"环境光整体倍率"用；倍率归 `ibl_intensity`/`sky_intensity`。
19. **运行实例锁 exe**：运行中的旧实例会让 `cargo build` 链接失败（os error 5）；改动前先 WM_CLOSE，确认退出码为 0。

## 5. 边界

- **不产生**：世界空间缓存、cell/patch、Volume、射线、BLAS/TLAS、motion vectors、屏幕 gather、SCGI 开关（9.4～9.8）。
- **不做**：SSR、局部反射探针、AO 本体、烘焙/lightmap、云层、**大气透视/体积雾**（已登记[外置拓展条目 17](../../外置拓展需求/外置拓展需求.md)）、自动曝光/pre-exposure 补偿、多相机、egui 面板。
- **不主张**：镜面间接（只做天空环境高光）、方向性遮蔽、室内天空遮挡、IBL 的多重散射补偿（9.2.7 裁决，默认不开）、逐帧捕获（只做脏标记 + 节流）。
- **不改**：阴影链、后处理链、G-buffer 布局、GPU-driven 闭环、光源常驻表。
- **透明层**：本段接 IBL，但不接 SCGI；9.7 仍只改不透明合成。

## 6. 材料

- [步骤 9 README](../README.md)——步骤索引、八段顺序与开工前置。
- [八段拆解：天空盒、IBL 与表面缓存全局光照](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§1 选型、§2 9.2 节（交付/判定线草案）、§5 目录组织。
- [9.1 段 README](../9.1-程序化天空盒/README.md)与[物理天空追加段](../9.1-程序化天空盒/施工计划：物理天空（9.1追加）.md)——捕获源的现行形态、相机海拔假设、太阳盘与米氏峰的语义。
- [9.1.6 物理天空模型与参数面](../9.1-程序化天空盒/9.1.6-物理天空模型与参数面.md)——`SkySettings` 字段、默认常数、亮度标定与与 HDRP 的三处差异。
- [9.1.7 判定线与计量（物理天空）](../9.1-程序化天空盒/9.1.7-判定线与计量（物理天空）.md)——天空开/关成本读数与回归口径，9.2 的 A/B 基线。
- [程序化天空盒算法原理](../9.1-程序化天空盒/程序化天空盒算法原理.md)与[渲染流程](../9.1-程序化天空盒/程序化天空盒渲染流程.md)——`sky.wgsl` 的积分结构、帧内位置与 9.2 接入点。
- [Filament BRDF参考](../../../笔记/光照与材质/Filament%20BRDF参考.md)——§4 直接光 BRDF 与 `energyCompensation`、§6 的 DFG LUT 归属、§6 第 6 条预曝光口径。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——环境 cubemap 捕获、太阳圆盘排除、`GlobalProbe` 更新条件（9.3 的依据）。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——探针的 5D 光照场、L1/L2、16 桶与 L2 的分工。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——`DistanceFallback` 与全局探针的定位（9.3/9.7 的边界）。
- 本工程现状锚点（2026-11-23 核实）：`shaders/common/sky.wgsl`（`sky_radiance(dir, include_sun)`，不声明绑定）、`shaders/scene/gbuffer/gbuffer_composite.wgsl`（环境项与调试模式）、`shaders/scene/transparent/transparent_forward.wgsl`（透明环境项）、`shaders/common/frame.wgsl`（UBO 304B）、`src/render/resident/tables/frame_uniform_table.rs`（布局镜像与断言）、`src/render/passes/lighting/composite_pipeline.rs` 与 `src/render/passes/transparent/transparent_pipeline.rs`（现有 4 组 set）、`src/render/passes/visibility/hiz.rs`（格式能力查询与 mip 视图模板）、`src/render/core/record/barrier.rs`（子资源范围缺口）、`src/driver/host/init.rs:114`（`SetLayouts` 登记）。
