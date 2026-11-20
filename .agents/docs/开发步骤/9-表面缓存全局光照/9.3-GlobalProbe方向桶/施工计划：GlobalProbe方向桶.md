# 施工计划：9.3 GlobalProbe 方向桶

> 2026-11-20 立案（纯文档，重规划）。对应[步骤 9 README](../README.md)、[八段拆解 §2 9.3](../施工计划：天空盒、IBL 与表面缓存全局光照八段拆解.md)与[路线图步骤 9 表](../../学习目标实现步骤.md)。本文管 9.3 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：📝 施工计划已立案，未开工（2026-11-20）**。

**一句话说明**：从 9.2 的天空 cubemap 生成 16 桶 × RGBL1 的 GlobalProbe，按法线选桶求值，并用 A/B 对照 L2；本段不产生空间缓存、不发射射线、不替代 L2 作为默认（除非数据支持）。

## 0. 立案期裁决（施工可推翻但须记录）

### 0.1 定位：方向类别 LUT + 与 patch 同构的兜底

9.2 的漫反射是单个 SH-L2：空间不变、方向连续、108B。9.3 的 GlobalProbe 是：

```
16 个方向桶（4×4 八面体）× 每桶一套 RGBL1
查询：法线 → 八面体方格 → 桶索引 → 该桶的 E(n)=l0+l1·n
```

它有两个明确用途：

1. **与 patch 同构的兜底**：9.5 的 patch 池、9.7 的屏幕 gather 都用 RGBL1；GlobalProbe 用同一种数据格式，9.7 在 Volume 外可以无缝回退。
2. **方向类别 LUT**：比单个 L2 更"局部"的分段近似，能保留地平线/太阳侧的对比；代价是桶边界硬切换、无插值、可能有接缝。

**它不是**：空间缓存、遮挡求解器、位置变化的来源，也不是"比 L2 更准"的自动结论。9.3 的判定线是 A/B，不是"桶越多越好"。

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
- **能量自检**：均匀天空辐亮度 `L` → 每桶在桶中心方向的 `E(n) = πL`，与 9.2 的 L2 口径一致。注意每桶只采一个半球，半球的 SH **L1 不一定为零**（半球本身有方向性）；不要把 `l1≈0` 当判据，只核对 `E(bucket_center)`。

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
    let p = min(vec2u(vec2f(res - 1u) * octahedral_sphere_to_square(n)), vec2u(res - 1u));
    return p.y * res + p.x;
}

fn bucket_index_to_direction(index: u32, res: u32) -> vec3f {
    let p = vec2f(f32(index % res), f32(index / res));
    return octahedral_square_to_sphere((p + vec2f(0.5)) / f32(res));
}
```

- 约定：`res = 4`，索引 `y*4 + x`；桶中心取 `(x+0.5, y+0.5)/4` 反投影，和 Unity `GlobalProbe.hlsl:5-16` 同式。
- 坑：`select` 的 true/false 顺序不要写反；`oct_wrap` 的符号决定下半球四个角的方向，写反会让桶方向镜像。
- 这份片段在 9.3 只服务 GlobalProbe；9.5 起 patch 的 16 桶直接 import 同一份，不改写。

### 0.4 存储与查询：4×vec4/条，1KB

- 每条 RGBL1 存 4 个 `vec4`：`l0.xyz`、`l1x.xyz`、`l1y.xyz`、`l1z.xyz`，w 写 0。
- 16 条 × 64B = **1KB**。Unity 用 12 floats/条 = 48B/条、768B；本工程按 WGSL `vec3` 的 16B 对齐取 64B/条，差 256B，可忽略；换来的是读写不用手工拆包。
- 查询：

```
idx = direction_to_bucket_index(n, 4) * 4
E(n) = probe[idx+0].xyz + probe[idx+1].xyz·n.x + probe[idx+2].xyz·n.y + probe[idx+3].xyz·n.z
```

- 投影与求值的分量顺序必须一致（本工程统一 x/y/z；Unity 的 `RGBL1` 是 y/z/x，不要照抄它的分量顺序）。
- buffer 是 storage buffer，挂在 9.2 的 IBL set 上（b4）；composite 与 transparent 共用。

### 0.5 模式切换：L2 主路径，16 桶 A/B

- 9.2 已在帧 UBO 预留 `ibl_mode`（272→288 的字段）。9.3 定义：
  - `0 = SH-L2`（9.2 的默认）；
  - `1 = GlobalProbe 16 桶`。
- 不透明合成与透明前向都按 `ibl_mode` 选漫反射求值；高光不受影响（仍走 9.2 的预滤波 cubemap）。
- 9.7 的屏幕 gather 在 Volume 外回退时，优先用 GlobalProbe（与 patch 同格式）；Volume 内仍走 patch。
- **默认值由 A/B 决定**：先用同一个天空、同一个 `ibl_intensity`，只在 `sh_l2`/`probe16` 之间切；记录画面差异、接缝、崩溃/NaN。若 16 桶没有可量化收益或有明显接缝，保留 L2 默认，16 桶只作 9.7 的兜底通路——这也是本段的价值，不丢。

### 0.6 更新：复用 9.2 的捕获脏标记

- GlobalProbe 与 SH-L2、预滤波共用同一个 `sky_hash` 与节流窗口；捕获重烘时一起重算 16 桶，不做独立的每帧更新。
- 512 次 cubemap 采样 + 16 次归约，成本远低于预滤波；不需要单独的质量档。
- 太阳仍不在 cubemap 里；GlobalProbe 只表示天空的低频环境，不含太阳直射。

### 0.7 BRP 与调试

- `ibl_set` 扩 `{"diffuse_mode": "sh_l2" | "probe16"}`；回显当前模式。
- `ibl_stats` 增加：`diffuse_mode`、16 桶的摘要（例如 ±X/±Y/±Z 六个方向的 `E(n)`，不打印全部 64 个分量）、最近 `sky_hash`。
- 可选调试档 `mode 11 = 桶可视化`：按法线选桶，输出桶索引/桶中心方向颜色；用来肉眼核对方向映射与接缝。若加，`RenderMode::ALL` 从 11 项扩到 12 项。
- egui 面板不接；BRP 够判定线。

## 1. 任务拆分

| 任务 | 交付 | 落点 | 状态 |
|---|---|---|---|
| 9.3.1 八面体公共片段 | `octahedral.wgsl`：`oct_wrap`、球↔方、`direction_to_bucket_index`、`bucket_index_to_direction`；CPU 侧镜像单元测试（往返 + 六轴落桶） | `shaders/common/octahedral.wgsl`（新）、`src/render/passes/ibl/` 或 `src/common/` 的测试 | ⬜ |
| 9.3.2 16 桶 L1 投影 | `global_probe.wgsl`：16 组 × 32 样本、组内归约、`2π/32` 归一化、`A0/A1` 卷积、折叠 L1；写 16×4 vec4 buffer | `shaders/scene/ibl/global_probe.wgsl`（新）、`src/render/passes/ibl/` | ⬜ |
| 9.3.3 查询与模式切换 | `ibl_mode` 分支；composite/transparent 的漫反射求值；IBL set b4 buffer | `shaders/common/ibl.wgsl`、`gbuffer_composite.wgsl`、`transparent_forward.wgsl`、`src/render/passes/ibl/` | ⬜ |
| 9.3.4 BRP 与调试 | `ibl_set` 的 `diffuse_mode`；`ibl_stats` 摘要；可选 mode 11 桶可视化 | `src/remote/features/ibl.rs`、`src/render/frame/render_mode.rs` | ⬜ |
| 9.3.5 判定线与 A/B | 桶方向正确性、均匀天空自检、L2 vs 16 桶读图、接缝/收益记录、回归矩阵 | 记录文件随施工产出 | ⬜ |

### 任务间依赖

- 9.3.1 先做（映射是投影与查询的共同前提）；CPU 镜像测试可与 shader 并行。
- 9.3.2 依赖 9.2.1 的源 cubemap 与 9.3.1 的桶中心方向；9.3.3 依赖 9.3.2。
- 9.3.4/9.3.5 最后；A/B 必须在同一天空/同一强度下跑。

## 2. 判定线（照八段拆解 §2 9.3 细化）

1. **方向映射正确**：CPU 镜像单测覆盖随机方向的球→方→球往返、六轴（±X/±Y/±Z）落桶；shader 侧用 mode 11 或数值回读核对同一组方向。
2. **均匀天空自检**：均匀天空 `L` 下每桶在桶中心方向的 `E(n)=πL`（半球 L1 不为零是正常的）；16 条 buffer 有限值、无 NaN。
3. **查询方向正确**：非均匀天空（太阳侧亮）下，法线转向亮侧时 `E(n)` 变大；桶边界处只有预期内的硬切换，不出现镜像/错位。
4. **A/B 有数据**：同一天空/强度下 `sh_l2` vs `probe16` 的读图差异、接缝、成本记录；结论写"默认选谁、为什么"，不写"16 桶更准"这种先验结论。
5. **兜底通路可用**：`ibl_mode=1` 在不透明/透明都能跑；9.7 的 Volume 外回退可以只换查询函数，不换数据格式。
6. **更新跟随**：天空参数变化后，GlobalProbe 在 9.2 的节流窗口内重算；静止时不重算。
7. **回归**：既有判定线零回退（8.2.5 五脚本）、库测试、零 VUID、`clippy` 全净、`fmt --check` 全绿、WM_CLOSE exit 0。

## 3. 测试方案

- **场景**：zero_day（天空渐变明显）+ DamagedHelmet/金属球（高光对照）；Sponza 只作室内漏光记录，不作方向判定。
- **启动**：`cargo run -p ash_renderer --release -- --scene zero_day/zero_day_measure_one.glb`；BRP 端口 15702。
- **BRP 配方**（`tools/brp.py raw`）：
  - L2：`--method ash_renderer/ibl_set --params-json '{"diffuse_mode":"sh_l2"}'`
  - 16 桶：`--method ash_renderer/ibl_set --params-json '{"diffuse_mode":"probe16"}'`
  - 定向天空：`--method ash_renderer/sky_set --params-json '{"zenith":[80,160,400],"horizon":[600,500,400],"ground":[50,40,30],"sun_gain":4.0}'`
  - 读数：`--method ash_renderer/ibl_stats`
- **数值自检**：`ibl_stats` 读 ±X/±Y/±Z 六方向的 `E(n)`；均匀天空下应全部 `πL`；非均匀天空下应随方向单调变化。
- **视觉验证**：PrintWindow 抓图 + 当前会话模型读图；取证落 `.temp/9.3/`，结论与截图路径入施工记录。
- **构建**：`cargo fmt --check -p ash_renderer`、`cargo clippy -p ash_renderer --all-targets`、`cargo test -p ash_renderer`；VUID 与 WM_CLOSE 退出码。

## 4. 风险与坑

1. **八面体映射写反**：`oct_wrap` 的符号、`select` 的 true/false 顺序、下半球四角方向，任何一处反了都会让桶镜像。CPU 单测先过，再接 GPU。
2. **桶索引与 patch 桶位漂移**：9.5 起 cell/patch 也用同一套桶；本段必须把函数抽到 `octahedral.wgsl`，禁止复制一份改改变量名。
3. **采样半球与桶形状不一致**：桶是方格、采样是半球，邻近桶重叠；这是已知近似，不要试图用"严格球冠"修，会把 Unity 对照打乱。
4. **归一化/卷积顺序**：先乘 `2π/32`，再余弦卷积，再折叠；顺序错了能量会差 `π` 或 `2π`，用均匀天空自检抓。
5. **分量顺序**：本工程统一 x/y/z；Unity 的 `RGBL1` 是 y/z/x。照抄 Unity 的投影代码时别连分量顺序一起抄。
6. **RGBL1 对齐**：WGSL `vec3` 16B 对齐，4×vec4/条 = 64B；如果为了省 256B 手工拆 12 floats，读写会更容易错，不值得。
7. **A/B 不公平**：两次对照必须同一天空、同一 `ibl_intensity`、同一机位；不要一边用 L2 一边偷偷改天空。
8. **接缝**：桶边界硬切换，低对比天空可能看不出来，高对比天空会有条带；记录，不当 bug 修。
9. **`ibl_mode` 与 9.7 的兜底语义**：9.7 Volume 外回退优先用 GlobalProbe；Volume 内仍走 patch。本段只建数据与查询，不接 Volume。
10. **BRP 参数校验**：`diffuse_mode` 只收两个字符串；错误回 `BrpError` 不 panic。
11. **运行实例锁 exe**：同前，改动前 WM_CLOSE。

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
- 本工程现状锚点：`shaders/common/frame.wgsl`（9.2 后的 UBO）、`src/render/resident/tables/frame_uniform_table.rs`（`ibl_mode` 字段）、`shaders/scene/gbuffer/gbuffer_composite.wgsl` 与 `shaders/scene/transparent/transparent_forward.wgsl`（漫反射求值点）。
