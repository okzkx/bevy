# 表面缓存辐照度更新与多跳：从每帧一次 dispatch 到 patch 表

> 面向下一个读 9.6 代码的人：这篇走读"一帧里辐照度是怎么算出来、写到哪、谁读它"，
> 以及为什么这样切。结论与数字在[段 README](README.md) 与各任务施工记录里，这里只讲流程。
> 段内引入的全部代码见文末[附录](#附录代码改动解析)（`git diff --stat` 实数：32 文件、
> +3154 / −234 行）。

## 1. 全景：四张账 + 一条 dispatch 链

9.5 建好了"哪里需要辐照度"（世界空间格子 + patch 池），9.6 负责"辐照度是多少"。

| 账 | 存在哪 | 谁写 | 谁读 |
|---|---|---|---|
| patch 几何/统计 | `patch_geometry` / `patch_statistics`（每槽 32B / 48B） | 播种、bounce 分配、估计（统计） | 估计、视图、探针 |
| **辐照度双份** | `patch_irradiance0` / `patch_irradiance1`（每槽 64B，**求值式** RGBL1） | 估计（每帧只写一份，按帧奇偶） | 估计的自查询（读另一份）、视图、9.7 的屏幕查表 |
| 光源侧清单 | `gi_lights`（紧凑灯表）/ `light_samples`（每帧 128×64B） | 宿主写灯、清单 dispatch 写采样 | 估计的重连接取样 |
| 自发光三角形 | `emissive_triangles`（每帧重建 32768×64B） | 重建 dispatch | 估计的面积采样 |

一帧的 ④.5 段（G-buffer 之后、合成之前）录 **6 条 compute**，顺序即依赖：

```
scrolling → eviction → defrag × N → patch_alloc(播种)
   → emissive_build → light_sampling → estimation → bounce_alloc
   →（段末 compute→片元屏障）→ ⑧ 段视图
```

前四个是 9.5 的（腾空间 → 分配）；后三个是 9.6 新增的，加一个"把请求变成 patch"的分配器。
`estimation` 是唯一读 patch 表并写辐照度的；其余都是喂给它或消费它的。

## 2. 新文件分工（一层一句话）

| 文件 | 一句话 |
|---|---|
| `shaders/common/gi_sampling.wgsl` | 逐 patch 的低差异序列 + 均匀半球 + RGBL1 投影/余弦卷积折叠（纯函数） |
| `shaders/common/shadow.wgsl` | CSM 采样（从合成 shader 抽出）+ 低档 5 tap PCF（估计专用） |
| `shaders/common/gi_light_sample.wgsl` | 光源侧采样记录（64B）与 1/pdf 常量 |
| `shaders/common/gi_emissive.wgsl` | 自发光三角形记录（64B）与表容量 |
| `shaders/common/gi_bounce.wgsl` | bounce 请求记录（48B）+ 三个计数槽号 |
| `shaders/common/gi_ring.wgsl` | `ring_append`（播种与 bounce 分配共用的池水位逻辑） |
| `shaders/scene/gi/estimation.wgsl` | 三路取光 + 多跳自查询 + 时域混合 + 写回（段内主体，473 行） |
| `shaders/scene/gi/light_sampling.wgsl` | 灯位向球面打射线，产清单 |
| `shaders/scene/gi/emissive_build.wgsl` | 对象槽展开自发光三角形 |
| `shaders/scene/gi/bounce_alloc.wgsl` | 消费请求，补 rank 1 的 patch |
| `src/render/passes/gi/estimation.rs` | 六个录制束 + 组数 + **预算换算**（纯函数，带单测） |
| `tools/gi_estimation_test.py` | 六条判定线（自启宿主、读图、探针、退出码） |

## 3. 逐时刻走读

### 3.1 帧首（宿主侧，`prepare_gi`）

1. **读 ring 水位**（48B 主机读）拿到活跃 patch 数 → `plan_estimation_budget` 算出本帧的
   **样本档**（1/2/4/8）与**稀释步长**（1..64）。档位先降、降到 1 还超才稀释。
2. **写光源紧凑表**：`LightTable::active_slots()` → 活灯记录按紧凑顺序写进 b10（16 KiB/帧）。
   为什么不用 cluster 的灯表：那张表是持久分配的环形槽，槽号不连续，而清单的 1/pdf 需要
   "0..N-1 连续"的活灯表。
3. **写 GI UBO**（304B）：几何/级联偏移/帧号 + 9.6 的两组估计字段（样本档、预热、多跳、
   清单长度、短滞后、自发光强度、活灯数、清单条数、稀释步长、预算用的活跃数）。
4. 清本槽计数（16 字，其中 7 个是 9.5 的分配账、7 个是 9.6 的估计账）。

### 3.2 ④.5 段（GPU）

**A. 自发光重建**（`emissive_build`，每线程一个对象槽）
对象有几何且材质自发光 → 逐三角形算世界空间顶点与面积 → `atomicAdd(counters[14])` 取槽位 →
写 64B 记录（p0+面积 / e1 / e2 / 辐亮度）。超容量截断但计数照加（读数能看出超了多少）。

**B. 光源侧清单**（`light_sampling`，每线程一条）
`idx % 活灯数` 摊灯 → 球面均匀方向 → 从灯位打 `tmax = 作用半径` 的射线 → 命中就记
位置/法线/反照率/距离/`1/pdf = 4π·灯数`。

**C. 估计**（`estimation`，每线程一条 patch）

```
活跃判据（patch→cell 反查非 INVALID）
  → 稀释判据（hash(patch) % stride == frame % stride）
  → 预热/稳态的样本区间（样本数、序列起点）
  → ① 半球迹线：命中 → albedo/π·(方向光·NoL·低档PCF + 多跳缓存E)；未命中 → 天空辐亮度
  → ② 光源清单重连接：隐面点必须在前半球 → 补重连接射线 → 面积↔立体角 Jacobian
  → ③ 自发光面积采样：均匀抽三角形 + 解析面积权重 → 阴影射线
  → 三路各自归一化后合流 → 余弦卷积折基常数（求值式）
  → 时域：短期均值/方差 → 漂移权重 → lerp(0.995, 短滞后) 或 精确累计平均
  → 写 ping-pong 的另一份 + 更新统计（mean / variance / update_count）
```

三路的归一化**不能混**：半球那路是 `2π/N`（pdf = 1/2π），另两路自带 `1/pdf` 在样本里，
只再除样本条数——一开始把点光那路加在 `2π/N` 缩放之前，整段被放大 2π。

**D. bounce 分配**（`bounce_alloc`，每线程一条请求）
请求里的 cell 已经有 patch / 法线非法 → 跳过；用 `cell_marks` 的帧号纪元标签与播种争同一份
分配权（同帧同 cell 只一次）→ `ring_append` 取槽 → 写几何、零种子、rank 1。

### 3.3 ⑧ 段（视图）

`patch_view` 解析像素所属 cell/bucket → patch → 按视图号调色。9.6 加 5 个子视图：9 样本档与
稀释、10 辐照度 `E(n)`、11 方差、12 更新次数、13 rank。10/11 是逐 patch 连续量（有层次）；
9/12/13 在收敛场景里本来就趋近单色（它们的用途是"有没有异常值"）。

### 3.4 谁读这份辐照度

- **本段**：`cache_evaluate`（多跳）读**另一份**（上一帧），视图读**本帧写的那份**——
  两者刻意不同：前者要"上一帧的传播结果"，后者要"这一帧算出了什么"。
- **9.7（下一步）**：屏幕侧只读 rank 0 的当前那份，乘 `albedo/π` 注入合成。

## 4. 失败与稳态路径

| 情形 | 行为 |
|---|---|
| `estimation = false`（默认） | 不录 5 条 dispatch，patch 表只有 IBL 种子，`update_count` 不涨 → 与 9.5 收官逐字节一致 |
| `gi_enabled = false` | ④.5 整段不录 → 与 9.3 逐位一致 |
| 无相机 / 0 实例 | 整段跳过（0 实例时 TLAS 从未构建，打射线是未定义行为） |
| 无活灯 | 清单条数 0，重连接那路不进循环 |
| 无自发光材质 | 三角形条数 0，面积采样那路不进循环 |
| 池满 | `ring_append` 返回 INVALID → 计数 +1、归还 marks 标签（允许后续帧重试） |
| 预算不够 | 先降样本档，仍不够再按 patch 哈希稀释；读数是"实际档位"不是设置值 |
| 通道滤波/NaN | 重连接贡献做有限性兜底；法线归一化前判零（否则 NaN 会经 Bloom 扩成黑斑） |

**稳态观察**（Sponza 档，60 fps）：屏幕播种 ~3320 条 patch；开多跳后 bounce 会补到
~13000～16000 条（每帧约 30 个新 cell、存活 4 秒后被心跳淘汰），射线数随之涨到预算附近，
默认 262144 预算下实际样本档降到 4。这是 9.8 压测要一起看的两个读数。

## 5. 与前后段的接口

- **9.5**：吃它的 patch 几何/统计/双份辐照度/`rank`/`request`/ring 水位；`frame.z`（bounce）
  由本段打开；淘汰侧的"rank 0→1"分支由此生效。
- **9.7**：屏幕侧查表读 rank 0 的**当前**那份（奇偶由 GI UBO 的 `frame.x & 1` 给出）；
  patch 级空间滤波与时域后滤波在 9.7 落地。
- **9.8**：本段的射线/样本/方差/清单长度、开关灯 lag、漏光案例直接进压测与局限对照；
  聚光锥角、自发光面积方差、per-patch 优先级与预算默认值的最终裁决在 9.8。

## 附录：代码改动解析

### 新增（14 个文件，+2211 行）

| 文件 | 行 | 改动要点与取舍 |
|---|---|---|
| `shaders/common/gi_sampling.wgsl` | +207 | Kronecker 2D（R2 + CP 旋转、周期 2²⁰）、low-bias hash、正交基、均匀/余弦半球、平衡启发式、RGBL1 投影与"余弦卷积+折基常数"（两个乘数 `π·Y00`、`2π/3·Y1`）。取舍：常量天空下 `l0 + l1·n` 必须等于 πL，单测钉住 |
| `shaders/common/shadow.wgsl` | +265 | CSM 从合成 shader 整段抽出 + 低档 5 tap PCF。取舍：合成端继续用同一定义（一份实现两处调用），顺带给 compute 也要的阶段位与屏障 |
| `shaders/scene/gi/estimation.wgsl` | +473 | 三路取光 + 自查询 + 时域 + 写回。取舍：三路各自归一化后合流；读另一份缓冲实现多跳；稀释按 patch 哈希 |
| `shaders/scene/gi/light_sampling.wgsl` | +118 | 灯位球面采样。取舍：无锥角信息 → 全向点光近似，锥外漏光记 9.8 |
| `shaders/scene/gi/emissive_build.wgsl` | +98 | 对象槽展开三角形。取舍：不做 CDF/前缀和（naga 30 的 workgroup 聚合坑 + 单线程串扫毫秒级），改均匀抽 + 解析权重 |
| `shaders/scene/gi/bounce_alloc.wgsl` | +86 | 请求 → rank 1 patch。取舍：与播种共用 marks 仲裁；零种子（下一帧被整体替换） |
| `shaders/common/gi_light_sample.wgsl` / `gi_emissive.wgsl` / `gi_bounce.wgsl` / `gi_ring.wgsl` | +20/+13/+24/+27 | 跨端记录与共用分配逻辑，注释写清"引用哪些全局名" |
| `src/render/passes/gi/estimation.rs` | +347 | 六个录制束 + 组数 + `plan_estimation_budget`。取舍：预算换算是**纯函数**（可单测），宿主只负责取活跃数与写 UBO |
| `tools/gi_estimation_test.py` | +340 | 六条判定线；判据写在注释里（含"为什么方差会涨"的解释） |

### 扩展（18 个文件，+943 / −234 行）

| 文件 | 改动要点 |
|---|---|
| `shaders/common/gi_cell.wgsl` | `GiUniforms` 扩到 304B（两组估计字段 + 光源字段）、`EstimateViewInput` 与 5 个子视图调色 |
| `shaders/scene/gbuffer/gbuffer_composite.wgsl` | −167 行：CSM 段改为 import 同一份，调用点传光方向 |
| `shaders/scene/gi/patch_view.wgsl` | 声明辐照度双份、`estimate_view_input`、两个调用点接估计视图 |
| `shaders/scene/gi/patch_alloc.wgsl` | `ring_append` 挪出到公共片段（−22 行） |
| `src/render/resident/tables/surface_cache.rs` | +215：UBO 288→304B（三段静态断言）、GI 集扩 b10～b13、四张新表与显存账、计数器 8/15 与 9～14 |
| `src/render/passes/gi/mod.rs` | +187：四个新管线配方、`GiPipelines` 的四个 `Option`（只在光线求交支持的机器上建）、录制顺序 |
| `src/driver/host/frame/gi.rs` | +133：紧凑灯表写入、预算换算、六集组装、四个录制束 |
| `src/remote/features/gi.rs` | +321：`gi_set` 八个估计参数与校验、`gi_stats` 四组读数、新口 `gi_estimate_probe` |
| `src/render/core/record/barrier.rs` / `shadow_map.rs` / `shadow_frame.rs` | 级联图 stage 扩 `COMPUTE`，进出场屏障补 `COMPUTE_SHADER` |
| `src/overlay/tool_window/gi.rs` | 视图表扩 13、估计参数控件、勾估计自动切 view=10 |
| `src/scene/mechanism/generation/test_entities.rs` / `remote/edit/shapes.rs` | `spawn_shape` 加 `emissive` 参数（判定线④ 的可复现锚点） |
| `src/driver/host/frame.rs` / `init.rs` / `render/mod.rs` / `layout_registry.rs` / `remote/mod.rs` | 接线：多传 5 个句柄与光源表、管线构造多一个"是否支持光线求交"、登记表 |
