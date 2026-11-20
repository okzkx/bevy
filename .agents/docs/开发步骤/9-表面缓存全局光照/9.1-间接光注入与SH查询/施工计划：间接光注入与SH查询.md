# 施工计划：9.1 间接光注入与 SH 查询

> 2026-11-20 立案（纯文档）。对应[步骤 9 README](../README.md)、[六段拆解 §2 9.1](../施工计划：表面缓存全局光照六段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.1 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：📝 施工计划已立案，未开工（2026-11-20）**。

**一句话说明**：把光照累积 pass 的间接项从"常量环境光"换成"可替换的辐照度数据源"——9.1 用单个全局 L1 SH 探针占位，钉死 `SampleIrradiance(P, N)` 接口、SH 余弦卷积与求值数学、GI 开关与 A/B 计量；零射线、零新 pass、零世界空间缓存，本段不产生 9.2 之后的任何东西。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.1 注入语义：替换环境项，不是叠加

```
GI 开：shaded = 直接光累加 + diffuseColor · (1/π) · E(n) · intensity
GI 关：shaded = 直接光累加 + albedo · ambient_color          // 步骤 8 原文，逐位不变
```

- `1/π` 直接复用 `Fd_Lambert()`（`shaders/common/brdf.wgsl`），与 Filament 直接光漫反射 lobe 同口径，不另写常数。
- `diffuseColor = albedo · (1 − metallic)`（`PixelParams.diffuseColor`）：金属不接间接漫反射。这和现行环境项用原始 `albedo` 不同，是 GI 开路径的有意修正；GI 关路径保持原样。
- AO 暂不乘间接项（与现行环境项同口径）。AO 是否接入留到 9.6 局限对照时裁决，本段登记为待决。
- **GI 关必须逐位一致**：开关分支里原样保留 `albedo * frame.ambient_color.rgb` 表达式，不写成 `albedo/π*(π*ambient)` 之类等价变形——浮点结果会不同。分支条件是 uniform（`gi_enabled`），无 warp 分歧。

### 0.2 数据载体：帧 UBO 扩 64B（208 → 272B），不新开 set

理由：探针是每帧更新的少量常量，帧 UBO 已有"每帧槽一份 + fence 保护 + 整块重写"的成熟纪律；新开 set 会牵动管线配方、布局登记与描述符池，收益为零。9.5 的世界缓存另行成 set，探针继续留在 UBO 作 DistanceFallback。

| offset | 字段 | 类型 | 说明 |
|---|---|---|---|
| 0 | `view_proj` | mat4 | 不变 |
| 64 | `dir_to_light` | vec4 | 不变 |
| 80 | `ambient_color` | vec4 | 不变（GI 关路径与透明层仍用） |
| 96 | `light_color` | vec4 | 不变 |
| 112 | `mode` | u32 | 不变 |
| 116 | `gi_enabled` | u32 | 0 关 / 1 开（新增，占用原对齐垫） |
| 120 | `gi_intensity` | f32 | 强度（新增，占用原对齐垫） |
| 124 | `_pad` | u32 | 对齐垫 |
| 128 | `inv_view_proj` | mat4 | 不变 |
| 192 | `camera_pos` | vec4 | 不变 |
| 208 | `gi_probe.l0` | vec3 + 4B 垫 | 辐照度常数项（新增） |
| 224 | `gi_probe.l1x` | vec3 + 4B 垫 | 沿 +X 的线性项（新增） |
| 240 | `gi_probe.l1y` | vec3 + 4B 垫 | 沿 +Y 的线性项（新增） |
| 256 | `gi_probe.l1z` | vec3 + 4B 垫 | 沿 +Z 的线性项（新增） |
| — | **总长** | **272** | `FRAME_UBO_SIZE` 208 → 272 |

坑：WGSL uniform 里 `vec3f` 成员按 16B 对齐，四个分量各占一个 vec4 槽（尾部 4B 写 0），不能按 12B 紧凑算偏移；Rust 侧 `FrameUniformTableLayout` 的 `offset_of!` 断言是三方同步的唯一闸门（WGSL / 布局镜像 / `pack_frame_uniforms`）。

### 0.3 SH 口径：存储"折叠基函数后的辐照度多项式"

本工程统一采用**直接求值形式**（每一步的推导写进 `shaders/common/gi.wgsl` 头注）：

```
E(n) = l0 + l1x·n.x + l1y·n.y + l1z·n.z        // 每通道各一套，共 12 个 f32
```

它和 Unity `RGBL1` 的存储形式只差一次常量折叠：

| 步骤 | Unity 源码 | 本工程 |
|---|---|---|
| 投影（样本累加） | `l0 += L·Y0`；`l1 += L·Y1·ω` | `gi_project_sample(L, ω)` 同式 |
| 余弦卷积 | `l0 *= A0`；`l1 *= A1`，`A0 = π`、`A1 = 2π/3` | `gi_cosine_convolve` 同式 |
| 折叠基函数 | 不求值不折（`Eval` 时再乘 Y0/Y1） | `gi_fold_basis`：`l0 *= Y0`、`l1 *= Y1`，之后求值只剩点积 |
| 求值 | `E = Y0·l0 + Y1·(l1·n)` | `E = l0 + l1·n`（已折叠） |

- `Y0 = 0.5·√(1/π) = 0.28209479`；`Y1 = 0.5·√(3/π) = 0.48860251`。
- **能量自检**：均匀环境辐亮度 `L`（全方向）→ 投影得 `4π·Y0·L`，卷积得 `π·(4π·Y0·L)`，折叠得 `l0 = πL`、`l1 = 0`，故 `E = πL`，间接项 `= diffuseColor·L`。这就是"均匀探针 = 环境项"的换算依据。
- 存储位置与直接光同域（**pre-exposure**）：合成 pass 末尾统一乘 `EXPOSURE`，探针里不再预乘曝光。本工程固定曝光，不做 pre-exposure 补偿；将来接自动曝光时在 9.5 补补偿项。
- 折叠与否只差每带一个正标量，投影/累加/滤波/时域混合全是线性运算，两种形式等价；选折叠形式的收益是求值只剩 3 次乘加、9.5 的 L1 压缩公式从 `l1·√3/(4·l0)+0.5` 化简为 `l1/(4·l0)+0.5`。
- 9.3 patch 池的存储格式在段立案时按同一形式钉明（12 个 f32/条，48B 压包）；9.1 只共享函数，不预设 patch 布局。

### 0.4 默认探针：跟随环境光

`GiSettings.probe = None`（缺省）时，本帧探针取 `l0 = π · ambient_color.rgb`、`l1 = 0`。效果：

- **GI 开但未设探针 ≈ 旧环境项**（介电体逐像素同形；金属面因 `diffuseColor` 变暗属预期）。这让"GI 开关"本身不引入画质突变，A/B 差异必须来自显式设置的 L1 或 intensity，判定线更干净。
- GI 关路径不读探针，硬线不受影响。
- Unity 的全局探针是 **16 方向桶 × RGBL1**（按法线方向选桶，768B），由环境 cubemap 在环境变化时重算，只在 Volume 外 + `DistanceFallback` 开启时被查询。9.1 用单探针退化形式（无 cubemap 可依，且注入端尚无 Volume）；9.5 若启用 DistanceFallback 再扩成 16 桶，查询函数与存储格式在 9.5 立案时钉明。

### 0.5 计量与调试视图

- **`gi_ms` 口径**：9.1 的注入在合成片元里，没有独立 pass；用 `perf_stats` 的 `gpu_lighting_ms`（T3→T4，含透明层）做开/关 A/B 差值，作为 9.1 的 GI 成本读数。独立 timestamp 槽等 9.5 有独立 GI pass 时再立（当前 query pool 8 槽已满）。
- **调试视图**（新增两档 `FRAME_MODE_*`）：
  - `mode 9 = GI 贡献`：`vec3(indirect · EXPOSURE)` 直出，看注入项形状；
  - `mode 10 = 探针辐照度相对倍率`：`E / (π · max(ambient_color, ε)) · 0.5`；0.5 灰 = 与原环境项等效，亮于 0.5 = 该法线方向拿到更多间接光。
  - `present_tonemap.wgsl` 已有 `frame.mode != 0` 跳过 bloom/tonemap 的旁路（`present_tonemap.wgsl:220`），新档自动继承；调试视图不做色调映射，高光截断属视图语义，判定线读结构差异不读绝对亮度。
- **透明层不接 GI**：`transparent_forward.wgsl` 保持 `albedo · ambient_color`（Unity 透明回退同款），9.5 再对照。

### 0.6 BRP 先行，egui 不接

新增 `gi_set`（写开关/强度/探针，回显当前值）与 `gi_stats`（只读：设置 + `gpu_lighting_ms` 均值），egui 面板本段不接（BRP 可用即够判定线，8.4 先例）。探针字段命名沿 `l0/l1x/l1y/l1z`（§0.3 的折叠形式，单位=辐照度）。

## 1. 任务拆分

| 任务 | 交付 | 落点 | 状态 |
|---|---|---|---|
| 9.1.1 SH 数学公共片段 | `GiProbeSh` 结构（vec3 l0/l1x/l1y/l1z）、`Y0/Y1/A0/A1` 常量、投影/累加/卷积/折叠/求值函数；头注写明"不声明绑定、参数化、归一化由调用方乘" | `shaders/common/gi.wgsl`（新） | ⬜ |
| 9.1.2 帧 UBO 扩列与宿主链 | `FRAME_UBO_SIZE` 208→272；`GiProbeSh` + `FrameUniformTableLayout` 断言；`FrameUniformsData.gi_*`；`pack_frame_uniforms`；`write_frame_ubo` 扩参；全仓 208B 引用注释同步 | `src/render/resident/tables/frame_uniform_table.rs`、`shaders/common/frame.wgsl`、`src/driver/host/frame/scene.rs`、`shaders/scene/gbuffer/gbuffer_write.wgsl` 注释 | ⬜ |
| 9.1.3 注入与调试视图 | 合成 pass `// #import gi` + 间接项替换环境项（开关分支保留原文）；背景/调试模式顺序不变；新增 `FRAME_MODE_GI`/`FRAME_MODE_GI_PROBE` 两档 | `shaders/scene/gbuffer/gbuffer_composite.wgsl`、`frame_uniform_table.rs` 常量、`src/render/frame/render_mode.rs` | ⬜ |
| 9.1.4 设置资源与 BRP 方法族 | `GiSettings`（enabled/intensity/probe）+ `Default`（跟随环境光）+ `resolve_probe(ambient)` + 单元测试；`init_resource`；`SceneInput` 接入；`gi_set`/`gi_stats` 注册 | `src/render/passes/gi/mod.rs`（新）、`src/render/passes/mod.rs`、`src/render/mod.rs`、`src/driver/host/mod.rs`、`src/driver/host/frame/scene.rs`、`src/remote/features/gi.rs`（新）、`src/remote/mod.rs` | ⬜ |
| 9.1.5 判定线与计量 | 开工基线截图（GI 关）；实现后 A/B 读图；`gpu_lighting_ms` 开/关各 600 帧成组；回归矩阵；施工记录 | 记录文件随施工产出 | ⬜ |

### 任务间依赖

- 9.1.1 → 9.1.2 可并行开工（前者只写公共片段，后者只改 UBO 布局），但 **9.1.3 必须等两者**（要同时引函数与字段）。
- 9.1.4 的 `resolve_probe` 依赖 9.1.2 的 `GiProbeSh` 类型；BRP 注册可先写壳。
- 9.1.5 最后跑；基线截图必须在任何代码改动前抓。

## 2. 判定线（照步骤 9 施工计划 §2/§9.1 细化）

1. **GI 关闭 = 步骤 8 逐像素一致（硬线）**：段开工前用当前 HEAD 抓基线截图（固定场景/机位/光照）；实现后 `gi_set {"enabled": false}` 同机位重抓，像素差为零（允许截图通道噪声，不接受画面结构差异）。证据入施工记录。
2. **GI 开启首次出现间接光差异**：`{"enabled": true, "l1y": [400,400,400]}` → 朝上法线变亮、朝下变暗（±13% 量级），读图与 GI 关同机位对照可辨；改 `l1x`/`l1z` 方向相应旋转。
3. **探针值随 BRP 即时反映**：`gi_set` 改 `l0`/`intensity`/L1 后下一帧生效；`gi_stats` 回显与 mode 10 视图同步变化。
4. **均匀探针自检**：`{"l0":[2513.27,2513.27,2513.27], "l1x":[0,0,0], "l1y":[0,0,0], "l1z":[0,0,0]}`（= π×800，环境光亮度 800）与 `enabled:false` 对照——介电体表面无可见差异；金属面变暗属预期并记录。
5. **透明层不接 GI**：GI 开/关切换时透明物体颜色不变（读图 + 代码核对透明 pass 未动）。
6. **回归**：既有判定线零回退（按 8.2.5 口径跑 hiz/lod/stream/fine/sponza 五脚本）、库测试、零 VUID、`clippy` 全净、`fmt --check` 全绿、WM_CLOSE exit 0。
7. **计量**：`perf_stats` 的 `gpu_lighting_ms` 在 GI 开/关各 600 帧的 p50/p95/mean；差值记入施工记录，并注明"含透明层、非纯 GI 成本"的边界。

## 3. 测试方案

- **场景**：Sponza（基座）+ zero_day（规模）；相机与光照沿用默认，不临时改场景。
- **启动**：`cargo run -p ash_renderer --release -- --scene zero_day/zero_day_measure_one.glb`；BRP 端口开发实例 15702。
- **BRP 配方**（`tools/brp.py raw`）：
  - 开：`--method ash_renderer/gi_set --params-json '{"enabled":true,"intensity":1.0,"l1y":[400,400,400]}'`
  - 回默认：`--method ash_renderer/gi_set --params-json '{"follow_ambient":true}'`
  - 关：`--method ash_renderer/gi_set --params-json '{"enabled":false}'`
  - 读数：`--method ash_renderer/gi_stats` / `--method ash_renderer/perf_stats`
- **视觉验证**：PrintWindow 抓图 + 当前会话模型读图（先写期望画面描述再读图）；取证落 `.temp/9.1/`，结论与截图路径入施工记录。
- **数值与构建**：`cargo fmt --check -p ash_renderer`、`cargo clippy -p ash_renderer --all-targets`、`cargo test -p ash_renderer`；运行日志抓 VUID 与 `WM_CLOSE` 退出码。
- **回归脚本**：`tools/hiz_test.py`、`tools/scene_lod.py`、`tools/scene_stream.py`、`tools/scene_fine.py`、`tools/scene_sponza.py`（按 8.2.5 总验收口径）。

## 4. 风险与坑

1. **UBO 三方同步**：`shaders/common/frame.wgsl`、`FrameUniformTableLayout`（`offset_of!` 断言）、`pack_frame_uniforms` 必须同改；`FRAME_UBO_SIZE` 208→272 后还要改 `frame.wgsl` 头注、`gbuffer_write.wgsl` 的 208B 注释。
2. **GI 关逐位一致**：开关分支内保留原表达式；不要用浮点等价变形，也不要把间接项先算好再 select（`select` 两边都会算，虽结果同但多花）。
3. **vec3 在 uniform 的对齐**：`l0/l1x/l1y/l1z` 各占 16B 槽（208/224/240/256），Rust 侧用 `[f32;4]` 行；`put_vec4(..., w=0)` 落位，别用 12B 步长。
4. **`RenderMode::ALL` 数组长度**：`[Self; 9]` → `[Self; 11]`，`label`/`from_name`/`as_u32`/`NAME_HINT` 四处同步；单元测试的往返断言会自动覆盖新档。
5. **嵌套 struct 导入**：`frame.wgsl` 需要 `// #import gi` 才能用 `GiProbeSh`；`gi.wgsl` 不得反向 import `frame`（循环引用=构建错误），函数全部参数化。
6. **调试视图值域**：mode 9/10 走 present 旁路、不做 tonemap；`indirect*EXPOSURE` 高光会截断，读图判定以结构差异为准。
7. **BRP 参数校验**：数组长度 3、有限值、`intensity ≥ 0`；错误回 `BrpError` 不 panic（系统错误处理规范）；`follow_ambient` 与其他探针字段互斥时以 `follow_ambient` 优先并回显。
8. **运行实例锁 exe**：运行中的旧实例会让 `cargo build` 链接失败（os error 5）；改动前先 WM_CLOSE，确认退出码为 0。
9. **默认探针的浮点口径**：`resolve_probe` 里 `π × ambient` 用 `std::f32::consts::PI`；shader 侧不再乘 π，避免双重卷积。

## 5. 边界

- **不产生**：世界空间缓存、cell/patch、Volume、射线、BLAS/TLAS、motion vectors、屏幕 gather、时域/空间滤波（9.2～9.5）。
- **不做**：IBL、反射（SSR/Reflection Probe）、AO 本体、烘焙/lightmap、自动曝光/pre-exposure 补偿、多相机、egui 面板。
- **不改**：透明层着色、阴影链、后处理链、G-buffer 布局、GPU-driven 闭环、光源常驻表。
- **不主张**：镜面间接、方向性遮蔽、Volume 外质量（9.5 的 DistanceFallback 才处理）。

## 6. 材料

- [步骤 9 README](../README.md)——步骤索引、六段顺序、参数基线与开工前置。
- [施工计划：表面缓存全局光照六段拆解](../施工计划：表面缓存全局光照六段拆解.md)——§1 选型、§2 9.1 节（交付/判定线草案）、§5 目录组织、§8 源码对照。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——§"射线怎么发"（`ProjectAndAccumulate` / `CosineConvolve`）与 §"屏幕端怎么消费"（L1 压缩、预曝光）；本段 SH 口径的源码依据。
- [SCGI 的间接光注入端](../../../笔记/光照与材质/SCGI间接光注入端.md)——注入公式与覆盖范围（只换不透明间接项）、全局探针与 9.5 世界缓存的职责边界、常见误解。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——发布状态、`DistanceFallback` 与全局探针的定位。
- [全局光照谱系调研](../../../笔记/光照与材质/全局光照谱系调研.md)——"查询端（档 2）→ 求交器 → 动态缓存"的顺序约束。
- Unity 本机源码：`...\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\Common.hlsl`（`SphericalHarmonics` 命名空间：`y0/y1Constant`、`Eval`、`CosineConvolve`）、`GlobalProbe.compute` / `GlobalProbe.hlsl`（16 方向桶 × RGBL1 的全局探针）、`...\SurfaceCacheGIRendererFeature\ScreenResolveLookup.compute`（Volume 外 + `DistanceFallback` 才查全局探针）。
- 本工程现状锚点：`shaders/scene/gbuffer/gbuffer_composite.wgsl`（环境项与调试模式）、`shaders/common/frame.wgsl`（UBO 208B）、`src/render/resident/tables/frame_uniform_table.rs`（布局镜像与断言）、`src/driver/host/frame/scene.rs`（`FrameUniformsData` 组装）、`src/render/frame/render_mode.rs`（模式名称表）、`src/remote/features/render_settings.rs`（BRP 设置方法族先例）。
