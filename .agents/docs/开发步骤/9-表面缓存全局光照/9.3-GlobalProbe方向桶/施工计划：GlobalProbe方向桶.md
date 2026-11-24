# 施工计划：9.3 GlobalProbe 方向桶

> 2026-11-20 立案（纯文档，重规划）；**2026-11-23 按 9.1 物理天空 / 9.2 同步修订**（常量天空自检改由 9.2.0 的 `sky_uniform` 提供，UBO 尺寸随 9.2 更新到 336B）；**2026-11-23 晚按 9.2 落地实况复核**（9.2 与 9.2.8 已收官：IBL 默认关且与天空开关串联、视向侧法线口径、IBL 集 b4 空位、两个读回口与 shader 编译机制——逐项见 §0.0 两张清单）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.3](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.3 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：✅ 收官（2026-11-23）。** 任务 9.3.1～9.3.5 全部落地，判定线七条全过；A/B 结论 = 默认保留 SH-L2、16 桶作 9.7 兜底（详见[9.3 段 README](README.md)与[判定线](9.3.5-判定线与计量.md)）。

**一句话说明**：从 9.2 的天空 cubemap 生成 16 桶 × RGBL1 的 GlobalProbe，按法线选桶求值，并用 A/B 对照 L2；本段不产生空间缓存、不发射射线、不替代 L2 作为默认（除非数据支持）。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.0 2026-11-23 同步修订清单（相对 2026-11-20 版）

| 项 | 旧口径 | 现行口径 | 依据 |
|---|---|---|---|
| 常量天空自检 | `sky_set` 设天顶/地平线/地面同色 | 9.2.0 的 `sky_uniform` 常量天空开关（物理天空没有这三个颜色参数） | [9.2 施工计划 §0.9](../9.2-天空盒IBL/施工计划：天空盒IBL.md) |
| 帧 UBO | `ibl_mode` 在 272→288 的扩列里 | 304→336 的扩列里（`ibl_mode` 偏移 308） | `frame_uniform_table.rs:39`（`FRAME_UBO_SIZE = 336`）与布局断言 150-152 |
| 定向天空配方 | `sky_set {"zenith":…,"horizon":…,"ground":…}` | 物理天空参数（`sky_set` 只收大气/地面反照率/太阳） | `src/remote/features/sky.rs:96-99` |
| 捕获源 | 「天空 cubemap」 | 物理天空 cubemap；只排太阳圆盘，米氏前向峰保留 | 9.1 追加段 |

**2026-11-23 按 9.2 落地复核清单**（相对上面的同步修订版；9.2 代码 `1d3e5f489`、9.2.8 补修 `269615dce`、收官文档 `47fd4f9ac`）：

| 项 | 同步修订版口径 | 落地后实况 | 依据 |
|---|---|---|---|
| 帧 UBO / `ibl_mode` | 304→336 扩列属 9.2 施工内容 | 已落地：336B、`ibl_mode`@308、`ibl_intensity`@312，`IblSettings.mode` 已逐帧出帧；shader 尚无读取方——9.3 只加求值分支，不扩 UBO | `frame_uniform_table.rs:39/150-152/178`、`driver/host/frame/scene.rs:204` |
| IBL 开关 | A/B 只切 `ibl_mode` | `IblSettings.enabled` 默认 **false**；帧 UBO 的 `ibl_enabled = ibl.enabled && sky.enabled`（9.2.8 天光联动）——A/B 前必须显式开 IBL（天空默认开） | `passes/ibl/mod.rs:51-57`、`driver/host/frame/scene.rs:202-204` |
| 查询法线 | 法线选桶求值 | 9.2.8 起 IBL 分支先做 `ibl_normal(n, v)`（视向侧翻转）再求值；probe 求值必须复用同一个翻转后的法线，否则双面几何上 A/B 比的是两条法线 | `gbuffer_composite.wgsl:253`、`transparent_forward.wgsl:79`、`common/ibl.wgsl` 的 `ibl_normal` |
| 常量天空自检 | `sky_uniform` 提供 | 已落地；`sky_radiance` 常量分支不吃 `sky_intensity`，可见天空与捕获同时变常量——`E=πL` 在 buffer 层严格成立，与 `ibl_intensity` 帧乘子无关 | `common/sky.wgsl:179-183`、`remote/features/sky.rs:249-291` |
| IBL 描述符集 | probe buffer 挂 b4 | b0 预滤波 cube / b1 采样器 / b2 DFG LUT / b3 SH 已占，**b4 空**，两条管线布局现成 | `passes/ibl/environment.rs:136-146`、`gbuffer_composite.wgsl:43-46`、`transparent_forward.wgsl:29-32` |
| 读回口 | `ibl_stats` 加 16 桶摘要 | `ibl_stats` 与 `ibl_census` 都已有"一次性拷贝 + fence"读回；SH 的 1KB 读回代码就是 probe 读回的模板 | `remote/features/ibl.rs:62-91`、`passes/ibl/environment.rs:696-702` |
| shader 编译 | 新公共片段 + 新 entry | build.rs 递归枚举非 `common/` 的 `.wgsl` 为 entry（自动生成 `shader_abi` 常量），`common/` 走 `// #import`（嵌套已用：`ibl.wgsl` → `brdf`）——9.3 不改 build.rs | `build.rs:1-11/43-70/129-173` |
| 投影线程形状 | 16 组各 32 线程、组内归约 | 9.2 两条 SH 管线都没用 workgroup 内存（partial 表 + 串行归约）；512 次采样成本可忽略——施工可简化为"一桶一线程、桶内 32 样本串行"（单 workgroup 16 线程），判定线不变 | `shaders/scene/ibl/sh_project.wgsl`、`sh_reduce.wgsl` |
| 求值函数形状 | `probe16_eval` 在 `ibl.wgsl` 里读 buffer | 本工程 shader 无 `ptr<storage>` 参数先例；按值参数设计（调用方按桶索引传该桶 4 个 `vec4`），桶索引留调用方 | `shaders/` 全量检索无 `ptr<storage` |
| 回归矩阵 | "8.2.5 五脚本" | 沿用 9.2.7 口径的五脚本：`hiz_test` / `scene_lod` / `scene_stream` / `scene_fine` / `scene_sponza` | `9.2-天空盒IBL/9.2.7-判定线与计量.md:20` |

### 0.1 定位：方向类别 LUT + 与 patch 同构的兜底

9.2 的漫反射是单个 SH-L2：空间不变、方向连续、144B（9×vec4）。9.3 的 GlobalProbe 是：

```
16 个方向桶（4×4 八面体）× 每桶一套 RGBL1
查询：法线 → 八面体方格 → 桶索引 → 该桶的 E(n)=l0+l1·n
```

它有两个明确用途：

1. **与 patch 同构的兜底**：9.5 的 patch 池、9.7 的屏幕 gather 都用 RGBL1；GlobalProbe 用同一种数据格式，9.7 在 Volume 外可以无缝回退。
2. **方向类别 LUT**：比单个 L2 更"局部"的分段近似，能保留地平线/太阳侧的对比；代价是桶边界硬切换、无插值、可能有接缝。

**它不是**：空间缓存、遮挡求解器、位置变化的来源，也不是"比 L2 更准"的自动结论，更不是 IBL 的替代层——它只换漫反射编码，高光（9.2 预滤波 cubemap + DFG LUT）与透明层仍走 IBL。9.3 的判定线是 A/B，不是"桶越多越好"。

### 0.2 算法：16 组 × 32 样本的半球 L1

```
for bucket in 0..16:
    dir = bucket_index_to_direction(bucket, 4)          // 桶中心方向
    for sample in 32:
        ω = uniform_hemisphere_sample(rng, dir)
        L = textureSampleLevel(env_cube, sampler, ω, 0) // 9.2 的源 cubemap，无太阳圆盘
        acc.l0  += L · Y0
        acc.l1  += L · Y1 · ω
    acc *= (2π / 32)                                    // 均匀半球密度的反权重
    cosine_convolve(acc)                                // A0 = π，A1 = 2π/3
    fold_basis(acc)                                     // 折叠成 E(n)=l0+l1·n
    probe[bucket] = acc
```

- 采样方向在**桶中心方向的半球**内均匀采样；桶是八面体方格，不是严格的球冠，邻近桶会有角度重叠——这是 Unity 同款近似，接受。
- 16 组各 32 线程，组内归约；总样本 512 次 cubemap 采样，成本可忽略，跟 9.2 的捕获脏标记同批重算。
- **能量自检**：`sky_uniform`（9.2.0）设常量辐亮度 `L` → 每桶在**桶中心方向**的 `E(n) ≈ πL`。推导：32 个半球样本是离散估计，l=0 项给 `πL/2`，L1 项因半球均值方向 = 桶中心 / 2 再补回 `πL/2`，两项叠加才是 `πL`（Unity 同款算法，已按 `GlobalProbe.compute` 的 `2π/32` + `CosineConvolve` 核对）。两点离散误差叠加，判定线给容差（建议 ±3%），不写逐位相等。注意每桶只采一个半球，半球的 SH **L1 不一定为零**（半球本身有方向性）；不要把 `l1≈0` 当判据，只核对 `E(bucket_center)`。物理天空本身没有均匀配置，所以这条判定线依赖 9.2.0 的测试开关，不靠调大气参数凑。
- **实现形状可简化**：16 组 × 32 线程 + 组内归约是 Unity 原形（`GlobalProbe.compute` 每桶一个 workgroup、groupshared 归约），但不是验收项。9.2 的两条 SH 管线都没用 workgroup 内存；512 次 cubemap 采样本身可忽略，施工也可改"一桶一线程、桶内 32 样本串行"（单 workgroup 16 线程），数据布局与判定线不变。

### 0.3 八面体映射：与 Unity 同式，抽成公共片段

映射必须和 9.5/9.7 的 cell/patch 桶位共用一份，否则"方向 → 桶"会在不同段漂移。统一放 `shaders/common/octahedral.wgsl`，照 Unity `Common.hlsl:161-182`：

```wgsl
fn oct_wrap(v: vec2f) -> vec2f {
    return (1.0 - abs(v.yx)) * select(vec2f(-1.0), vec2f(1.0), v >= vec2f(0.0));
}

fn octahedral_sphere_to_square(n: vec3f) -> vec2f {
    var p = n / (abs(n.x) + abs(n.y) + abs(n.z));
    if (p.z < 0.0) { p = vec3f(oct_wrap(p.xy), p.z); }
    return p.xy * 0.5 + 0.5;
}

fn octahedral_square_to_sphere(f: vec2f) -> vec3f {
    let g = f * 2.0 - 1.0;
    var n = vec3f(g.x, g.y, 1.0 - abs(g.x) - abs(g.y));
    let t = saturate(-n.z);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return normalize(n);
}

fn direction_to_bucket_index(n: vec3f, res: u32) -> u32 {
    // Unity 同式：方格坐标 × res（不是 ×(res−1)），截断后钳到 res−1。
    // 写成 ×(res−1) 会把桶边界整体压歪，方向→桶与桶→方向不再互逆。
    let p = min(vec2u(octahedral_sphere_to_square(n) * f32(res)), vec2u(res - 1u));
    return p.y * res + p.x;
}

fn bucket_index_to_direction(index: u32, res: u32) -> vec3f {
    let p = vec2f(f32(index % res), f32(index / res));
    return octahedral_square_to_sphere((p + vec2f(0.5)) / f32(res));
}
```

- 约定：`res = 4`，索引 `y*4 + x`；正向索引 = 方图坐标 × 4 截断后钳 `res−1`，桶中心取 `(x+0.5, y+0.5)/4` 反投影——与 Unity `GlobalProbe.hlsl:5-17` 同式（2026-11-23 对照源码核实；`volumeAngularResolution = 4` 见 `PatchUtil.hlsl:43`）。
- 坑：`select` 的 true/false 顺序不要写反；`oct_wrap` 的符号决定下半球四个角的方向，写反会让桶方向镜像；正向索引乘的是 `res` 不是 `res−1`（乘错后方向→桶与桶→方向不互逆，往返单测会抓到）。
- `saturate` 来自 `brdf.wgsl`（`ibl.wgsl` 已 `#import`）；若 `octahedral.wgsl` 被不引 brdf 的 entry 导入，就地写 `clamp(x, 0.0, 1.0)`，别假设环境自带。
- 这份片段在 9.3 只服务 GlobalProbe；9.5 起 patch 的 16 桶直接 import 同一份，不改写。

### 0.4 存储与查询：4×vec4/条，1KB

- 每条 RGBL1 存 4 个 `vec4`：`l0.xyz`、`l1x.xyz`、`l1y.xyz`、`l1z.xyz`，w 写 0。
- 16 条 × 64B = **1KB**。Unity 用 12 floats/条 = 48B/条、768B；本工程按 WGSL `vec3` 的 16B 对齐取 64B/条，差 256B，可忽略；换来的是读写不用手工拆包。
- 查询：

```
idx = direction_to_bucket_index(n, 4) * 4
E(n) = probe[idx+0].xyz + probe[idx+1].xyz·n.x + probe[idx+2].xyz·n.y + probe[idx+3].xyz·n.z
```

- 投影与求值的分量顺序必须一致（本工程统一 x/y/z；Unity 的 `RGBL1` 是 y/z/x——`Common.hlsl:47-56` 的 `l1s` 序与 `Eval` 都按 y/z/x，不要照抄它的分量顺序）。
- **基函数常数只乘一次**：`fold_basis` 把 `Y00 = 0.282095` 与 L1 基常数 `0.488603` 乘进存储系数（同 9.2 把余弦卷积 `A_l` 折进 SH 的做法），于是查询式只剩 `l0 + l1·n`；若投影端不折，就得在求值端乘——两处都乘会让 `E` 差一个常数因子，`sky_uniform` 自检会抓到。
- buffer 是 storage buffer，挂在 9.2 的 IBL set 上（b4）；composite 与 transparent 共用。

### 0.5 模式切换：L2 主路径，16 桶 A/B

- 9.2 已在帧 UBO 预留 `ibl_mode`（304→336 扩列，偏移 308）。9.3 定义：
  - `0 = SH-L2`（9.2 的默认）；
  - `1 = GlobalProbe 16 桶`。
- 不透明合成与透明前向都按 `ibl_mode` 选漫反射求值；高光不受影响（仍走 9.2 的预滤波 cubemap）。
- **二选一，不是级联也不是叠加**：两次投影互相独立（都直接读源 cubemap），重烘时两份都算、都存（L2 144B、probe 1KB，成本可忽略）；着色时按 `ibl_mode` 取其中一个。所以 `ibl_mode=1` 只是"绕过 L2 的求值"，不删数据、不改高光、可逐帧切回，A/B 不需要重烘。
- 9.7 的屏幕 gather 在 Volume 外回退时，优先用 GlobalProbe（与 patch 同格式的**漫反射编码**，仍属 IBL 基线）；Volume 内仍走 patch。高光始终走 9.2 的预滤波 cubemap，与兜底选择无关。
- **A/B 前置（9.2 落地后）**：`ibl_enabled` 默认关，且帧 UBO 里与 `sky_enabled` 串联（9.2.8：天空关 ⇒ 环境项回退常量 ambient）；对照前先 `ibl_set {"enabled":true}`（天空默认开），两次对照用同一 `ibl_intensity`、同一机位。
- **查询法线口径（9.2.8）**：两个模式必须共用同一个"经 `ibl_normal(n, v)` 翻到视向侧"的法线——桶选择也在翻转之后；否则双面几何（帘布/叶片）上 A/B 的差异来自法线而不是编码。
- **默认值由 A/B 决定**：先用同一个天空、同一个 `ibl_intensity`，只在 `sh_l2`/`probe16` 之间切；记录画面差异、接缝、崩溃/NaN。若 16 桶没有可量化收益或有明显接缝，保留 L2 默认，16 桶只作 9.7 的兜底通路——这也是本段的价值，不丢。

### 0.6 更新：复用 9.2 的捕获脏标记

- GlobalProbe 与 SH-L2、预滤波共用同一个 `sky_hash` 与节流窗口；捕获重烘时一起重算 16 桶，不做独立的每帧更新。
- 512 次 cubemap 采样 + 16 次归约，成本远低于预滤波；不需要单独的质量档。
- 太阳仍不在 cubemap 里；GlobalProbe 只表示天空的低频环境，不含太阳直射。

### 0.7 BRP 与调试

- `ibl_set` 扩 `{"diffuse_mode": "sh_l2" | "probe16"}`（字符串映射 `IBL_MODE_*` 常量，校验形状照 `set_render_mode` 的 `RenderMode::from_name`）；回显当前模式。
- `ibl_stats` 增加：`diffuse_mode`、16 桶的摘要（例如 ±X/±Y/±Z 六个方向的 `E(n)`，不打印全部 64 个分量）、最近 `sky_hash`；摘要从 probe buffer（1KB）一次性读回算，与 SH 读回同款。
- `ibl_census` 可选加 `probe: true`：回全部 16×4 个 float（全量取证口，判定线 1 的映射核对靠它）；与 `ibl_stats` 的摘要共用同一读回缓冲，不加新机制。
- 可选调试档 `mode 11 = 桶可视化`：按法线选桶，输出桶索引/桶中心方向颜色；用来肉眼核对方向映射与接缝。若加，`RenderMode::ALL` 从 11 项扩到 12 项（9.2 已用 9/10，下一号正好 11）。
- egui 面板：收官时未接（BRP 够判定线）；**2026-11-24 按用户要求追加**工具窗口"环境光照 IBL"页（开关/强度/编码二选一，见 [9.3.4 追加节](9.3.4-BRP与调试视图.md)），其余仍不接。

## 1. 任务拆分

| 任务 | 交付 | 落点 | 状态 |
|---|---|---|---|
| 9.3.1 八面体公共片段 | `octahedral.wgsl`：`oct_wrap`、球↔方、`direction_to_bucket_index`、`bucket_index_to_direction`；CPU 侧镜像单元测试（往返 + 六轴落桶） | `shaders/common/octahedral.wgsl`（新）、`src/render/passes/ibl/` 或 `src/common/` 的测试 | ✅ |
| 9.3.2 16 桶 L1 投影 | `global_probe.wgsl`：16 桶 × 32 样本（线程形状可简化为一桶一线程串行）、`2π/32` 归一化、`A0/A1` 卷积、折叠 L1；写 16×4 vec4 buffer（1KB，`BufferRole::DeviceStorage`）；新增捕获侧 set（b0 = mip0 cube、b1 = 采样器、b2 = probe buffer）并接进 9.2 的烘焙链（`sh_check` 之后），写→读屏障随 SH/stats 一起收口 | `shaders/scene/ibl/global_probe.wgsl`（新）、`src/render/passes/ibl/environment.rs` | ✅ |
| 9.3.3 查询与模式切换 | `ibl_mode` 分支（0 = `sh_irradiance`、1 = `probe16_eval`）；composite/transparent 两处漫反射求值都换来源，法线复用 9.2.8 翻转后的 `n`；IBL 帧集加 b4 绑 probe buffer（b0..b3 已占） | `shaders/common/ibl.wgsl`、`gbuffer_composite.wgsl`、`transparent_forward.wgsl`、`src/render/passes/ibl/environment.rs`（帧集加 b4） | ✅ |
| 9.3.4 BRP 与调试 | `ibl_set` 的 `diffuse_mode`；`ibl_stats` 摘要与 `ibl_census` 全量读回（可选）；可选 mode 11 桶可视化 | `src/remote/features/ibl.rs`、`src/render/frame/render_mode.rs` | ✅ |
| 9.3.5 判定线与 A/B | 桶方向正确性、`sky_uniform` 常量天空自检、L2 vs 16 桶读图、接缝/收益记录、回归矩阵 | 记录文件随施工产出 | ✅ |

### 任务间依赖

- 9.3.1 先做（映射是投影与查询的共同前提）；CPU 镜像测试可与 shader 并行。
- 9.3.2 依赖 9.2.1 的源 cubemap 与 9.3.1 的桶中心方向；9.3.3 依赖 9.3.2。
- 9.3.4/9.3.5 最后；A/B 必须在同一天空/同一强度下跑。

## 2. 判定线（照八段拆解 §2 9.3 细化）

1. **方向映射正确**：CPU 镜像单测覆盖随机方向的球→方→球往返、六轴（±X/±Y/±Z）落桶（正向索引 = 方图坐标 × res 截断，与 Unity 同式）；shader 侧用 mode 11 或 `ibl_census` 全量读回核对同一组方向（方向 → 桶 → 桶中心方向的夹角应在半个桶角内）。
2. **常量天空自检**：`sky_uniform` 设常量 `L` 下每桶在桶中心方向的 `E(n) ≈ πL`（32 个半球样本是离散估计，容差 ±3%；半球 L1 不为零是正常的）；16 条 buffer 有限值、无 NaN。
3. **查询方向正确**：非均匀天空（太阳侧亮）下，法线转向亮侧时 `E(n)` 变大；桶边界处只有预期内的硬切换，不出现镜像/错位。
4. **A/B 有数据**：同一天空/强度下 `sh_l2` vs `probe16` 的读图差异、接缝、成本记录；结论写"默认选谁、为什么"，不写"16 桶更准"这种先验结论。
5. **兜底通路可用**：`ibl_mode=1` 在不透明/透明都能跑；9.7 的 Volume 外回退可以只换查询函数，不换数据格式。
6. **更新跟随**：天空参数变化后，GlobalProbe 在 9.2 的节流窗口内重算；静止时不重算。
7. **回归**：既有判定线零回退（9.2.7 口径五脚本：`hiz_test` / `scene_lod` / `scene_stream` / `scene_fine` / `scene_sponza`）、库测试、零 VUID、`clippy` 全净、`fmt --check` 全绿、WM_CLOSE exit 0。

## 3. 测试方案

- **场景**：zero_day（天空渐变明显）+ DamagedHelmet/金属球（高光对照）；Sponza 只作室内漏光记录，不作方向判定。
- **启动**：`cargo run -p ash_renderer --release -- --scene zero_day/zero_day_measure_one.glb`；BRP 端口 15702。
- **BRP 前置**：`--method ash_renderer/ibl_set --params-json '{"enabled":true}'`（IBL 默认关；天空默认开，若被关过再 `sky_set {"enabled":true}`——9.2.8 起天空关会让 `ibl_enabled` 一起关）。
- **BRP 配方**（`tools/brp.py raw`）：
  - L2：`--method ash_renderer/ibl_set --params-json '{"diffuse_mode":"sh_l2"}'`
  - 16 桶：`--method ash_renderer/ibl_set --params-json '{"diffuse_mode":"probe16"}'`
  - 定向天空：`--method ash_renderer/sky_set --params-json '{"mie":20e-6,"sun_gain":4.0}'`（物理天空，只改米氏/太阳增益；风向与太阳由宿主场景给）
  - 常量天空：`--method ash_renderer/sky_uniform --params-json '{"enabled":true,"color":[800,800,800]}'`（9.2.0 的测试开关）
  - 读数：`--method ash_renderer/ibl_stats`（6 方向摘要）/ `ibl_census`（`probe: true` 全量）
- **数值自检**：`ibl_stats` 读 ±X/±Y/±Z 六方向的 `E(n)`；`sky_uniform` 打开时应全部 `πL`；物理天空下应随方向单调变化。
- **视觉验证**：PrintWindow 抓图 + 当前会话模型读图；取证落 `.temp/9.3/`，结论与截图路径入施工记录。
- **构建**：`cargo fmt --check -p ash_renderer`、`cargo clippy -p ash_renderer --all-targets`、`cargo test -p ash_renderer`；VUID 与 WM_CLOSE 退出码。

## 4. 风险与坑

1. **八面体映射写反**：`oct_wrap` 的符号、`select` 的 true/false 顺序、下半球四角方向，任何一处反了都会让桶镜像。CPU 单测先过，再接 GPU。
2. **桶索引与 patch 桶位漂移**：9.5 起 cell/patch 也用同一套桶；本段必须把函数抽到 `octahedral.wgsl`，禁止复制一份改改变量名。
3. **采样半球与桶形状不一致**：桶是方格、采样是半球，邻近桶重叠；这是已知近似，不要试图用"严格球冠"修，会把 Unity 对照打乱。
4. **归一化/卷积顺序**：先乘 `2π/32`，再余弦卷积，再折叠；顺序错了能量会差 `π` 或 `2π`，用 `sky_uniform` 常量天空自检抓。
5. **分量顺序**：本工程统一 x/y/z；Unity 的 `RGBL1` 是 y/z/x。照抄 Unity 的投影代码时别连分量顺序一起抄。
6. **RGBL1 对齐**：WGSL `vec3` 16B 对齐，4×vec4/条 = 64B；如果为了省 256B 手工拆 12 floats，读写会更容易错，不值得。
7. **A/B 不公平**：两次对照必须同一天空、同一 `ibl_intensity`、同一机位；不要一边用 L2 一边偷偷改天空。
8. **接缝**：桶边界硬切换，低对比天空可能看不出来，高对比天空会有条带；记录，不当 bug 修。
9. **`ibl_mode` 与 9.7 的兜底语义**：9.7 Volume 外回退优先用 GlobalProbe；Volume 内仍走 patch。本段只建数据与查询，不接 Volume。
10. **BRP 参数校验**：`diffuse_mode` 只收两个字符串；错误回 `BrpError` 不 panic。
11. **运行实例锁 exe**：同前，改动前 WM_CLOSE。
12. **正向索引乘 `res` 不是 `res−1`**：`floor(方图坐标 × res)` 再钳 `res−1`（Unity 同式）；乘 `res−1` 会把桶边界压歪、方向↔桶不互逆——往返单测与 `sky_uniform` 自检一起抓。
13. **基函数常数单点折叠**：`Y00` 与 `0.488603` 只在投影端折一次，求值式 `l0 + l1·n` 不再乘；两处都乘或都不乘，能量差常数因子。

## 5. 边界

- **不产生**：世界空间缓存、cell/patch、Volume、射线、屏幕 gather、SCGI 开关（9.4～9.8）。
- **不做**：空间插值、遮挡、位置变化、桶边界滤波、更高阶 SH、每帧更新。
- **不主张**：16 桶比 L2 更准；它只是另一种方向类别 LUT + 与 patch 同构的兜底。默认模式由 A/B 决定。
- **不改**：9.2 的高光路径、天空捕获、直接光、阴影、后处理、G-buffer。

## 6. 材料

- [步骤 9 README](../README.md)——步骤索引、八段顺序与开工前置。
- [八段拆解：天空盒、IBL 与表面缓存全局光照](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)——§1 选型、§2 9.3 节、§5 目录组织。
- [SH 与光照探针](../../../笔记/光照与材质/SH与光照探针.md)——GlobalProbe 16 桶与 Cell 16 槽的区别、16 桶 vs L2 的对照表。
- [Unity SCGI 实现](../../../笔记/光照与材质/Unity%20SCGI%20实现.md)——`GlobalProbe.compute` / `GlobalProbe.hlsl`、`Common.hlsl` 的八面体映射与 `RGBL1` 布局。
- [Unity SCGI 调研](../../../笔记/光照与材质/Unity%20SCGI%20调研.md)——`DistanceFallback` 的质量档与局限。
- [9.2 段 README](../9.2-天空盒IBL/README.md)与[9.2.8 现场复检补修](../9.2-天空盒IBL/9.2.8-高光火花、天空地平线噪点与天光开关联动.md)——本段的落地基线（IBL 默认关、天光联动、视向侧法线口径的来源）。
- 本工程现状锚点（2026-11-23 按 9.2 落地复核）：`shaders/common/frame.wgsl`（UBO 336B）、`src/render/resident/tables/frame_uniform_table.rs`（`ibl_mode`@308）、`shaders/scene/gbuffer/gbuffer_composite.wgsl` 与 `shaders/scene/transparent/transparent_forward.wgsl`（漫反射求值点与 group 4 b3/b4）、`shaders/common/sky.wgsl`（`sky_uniform` 分支）、`src/render/passes/ibl/environment.rs`（烘焙链与 IBL 集布局）、`src/remote/features/ibl.rs`（读回口）。
