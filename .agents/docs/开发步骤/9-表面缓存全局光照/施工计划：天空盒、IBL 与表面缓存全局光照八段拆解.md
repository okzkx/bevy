# 施工计划：天空盒、IBL 与表面缓存全局光照八段拆解（步骤 9）

> 2026-11-20 立案，同日按用户指令重规划（"9.1 程序化天空盒，9.2 天空盒 IBL，9.3 GlobalProbe 细化，再接 SCGI"）。对应[路线图](../学习目标实现步骤.md)步骤 9：在步骤 8 的 G-buffer 与光照累积地基上，先补天空与 IBL 环境基线，再做运行时动态 diffuse GI。**SCGI 选型仍是 2026-11-20 拍板的 Unity SCGI 骨架 + Kajiya 三件借用**（§1）；天空/IBL 的选型见 §1.5。本文管步骤 9 做什么、按什么顺序、验收什么；段目录与段级施工计划开工时立（一次只施工一段），"实际做了什么"开工后另立施工记录，不回写本文。
>
> **一句话说明**：先用程序化天空和标准 split-sum IBL 把"环境光"从常量色升级成有方向性的天空基线（9.1～9.2），再用 GlobalProbe 16 桶建立与 patch 同构的兜底（9.3），最后用"空间格子 + 方向桶 + 定长 patch 池"缓存世界空间表面辐照度、少量射线维护、屏幕侧查缓存→滤波→上采样→注入（9.4～9.8）；本步只主张间接漫反射，不主张 SSR/局部反射探针、AO 与烘焙。

## 0. 定位：现状与缺口

步骤 8 收官后的着色侧形状（核实 2026-11-20）：

- **G-buffer**：albedo / normal / 深度（世界位置深度重建）；motion vectors 附件已留位、未写内容。
- **光照累积 pass**：Filament BRDF、多光源常驻表（方向光 + 点/聚光）、CSM+PCSS 阴影采样。
- **输出链**：HDR → Bloom → Filament ACES → SMAA 直出；固定曝光 `EXPOSURE = 0.00016`。
- **环境光完全缺失方向性**：背景是清屏色（`CLEAR_COLOR`），没有天空盒；材质只有 `albedo × ambient_color` 的常量环境项，没有 IBL、没有天空高光、没有 GI；场景观感平灰（zero_day 归因已记）。
- **透明层**同样只有常量环境项，没有环境反射。
- **2026-11-23 现状补记**：9.1 已收官，背景改为物理天空；环境项仍是常量——本节其余内容（IBL/GI 缺口）仍然成立。

步骤 9 分两层换掉这个现状：

1. **环境基线（9.1～9.3）**：程序化天空 → 环境 cubemap → SH-L2 漫反射 + 预滤波高光 + DFG LUT；GlobalProbe 16 桶作为方向类别 LUT 与后续 patch 的兜底格式。
2. **世界空间 GI（9.4～9.8）**：在 IBL 基线上，用 Volume/cell/patch 缓存把"体积内的漫反射间接"换成逐位置/逐表面的估计；体积外保持 9.3 的漫反射编码（SH-L2 或 GlobalProbe，同属 IBL 基线）。

它不动步骤 5/6 的 GPU-driven 骨架，也不动步骤 8 的直接光与阴影链。**"环境项回退可逐位还原"是贯穿全程的红线**：9.2 之前 `ibl_enabled=false` + `sky_enabled=false` = 步骤 8；9.7 之后 `gi_enabled=false` = IBL 基线。

## 1. 选型结论

### 1.1 为什么不是 Kajiya

| 判据 | 说明 |
|---|---|
| 屏幕查询路径 | 光栅化管线需要一个"像素位置直接查"的接口。Unity 有（1/4 屏幕查缓存→去噪→上采样）；Kajiya 明确没有（缓存只被射线命中点查）。选它就得自己发明屏幕查询，最后仍回到 Unity 的形状 |
| 分配驱动 | Kajiya 的 output-sensitive 投票预设"每像素已有一条首弹射线"——那是路径追踪器的既有预算。延迟光栅化没有这条射线；为投票凭空加一条逐像素 ray 是净增成本 |
| 收益顺序 | 本工程步骤 8 无 IBL、无烘焙；步骤 9 先补 IBL（9.1～9.2）再上 SCGI。最缺的是稳定、可被着色器直接消费的间接漫反射；Unity 模型第一版就能给，Kajiya 要先有路径追踪器才闭环 |
| 硬件与工程 | 2060 的 `VK_KHR_ray_query` / `VK_KHR_acceleration_structure` 已实测可用，硬件光追不是障碍；Kajiya 是 Rust + rust-gpu，语言近但 compute 栈不通用；Unity 源码可逐行对照，实现与调试速度直接受益 |

### 1.2 借什么 / 不借什么

**借三样：**

1. **法线偏置**：播种与查询时沿法线偏移。在方向桶调好之前先用它压薄几何双面漏光，一比一的性价比。
2. **rank 不变式**：屏幕查询只读 rank 0（相机可见来源），任何"看着还行"都不作为放宽理由。
3. **投票分配器（二期备件）**：仅当大世界实测屏幕覆盖不足、bounce 预算提高仍补不上时，才在**粗级联**上引入；只换分配器，不动查询结构。

**不借**：每条目 ReSTIR 蓄水池（4×4 八面体 + 验证射线）、12 级 clipmap 配比、output-sensitive 作为主分配器。

### 1.3 参数基线（6GB 显存账）

| 阶段 | Res | 级联 | Size | 最细体素 | 覆盖 | cell 表（单张 / 双表） | patch 池 |
|---|---|---|---|---|---|---|---|
| 9.5 起步 | 32 | 4 | 128 m | 0.5 m | 128 m | 8 MiB / 16 MiB | 65536 条 ≈ 12 MiB |
| 大世界档 | 32 | 6 | 512 m | 0.5 m | 512 m | 12 MiB / 24 MiB | 同上（可提 2^18 ≈ 48 MiB） |
| 高品质候选（二期） | 64 | 4 | 128 m | 0.25 m | 128 m | 64 MiB / 128 MiB；按级联降桶 16/8/4/4 → 32 MiB / 64 MiB | 同上 |

- cell 表算法：`Res³ × 16 桶 × 级联数 × 4B`。**两张按 cell 计的表是 `CellPatchIndices`（16 个 uint 槽）与 `CellAllocationMarks`（分配仲裁）**；defrag 的 flip-flop 是 ring 配置的读/写，不是这两张表。
- **一期就用满 16 桶**（对齐 Unity 源码，便于逐行对照）；按级联降桶只在高品质档启用。
- 覆盖靠**加级联**而不是加分辨率：每加一级只多 `Res³ × 16` 个 cell（线性），不是立方增长。这与 Unity 官方"覆盖越大越要靠级联"一致。
- 起步参数与 Unity 默认体积（128 m / 4 级联 / 最细约 0.5 m）同档；其官方 251 测试场是 Size 20 / Res 128 / 1 级联（0.156 m），只作小范围质量对照，不照抄。
- 与步骤 7 的显存压力共存：Emerald 实测 4.6 GB 贴图已逼近 6 GB 悬崖；GI 全部结构控制在 100 MiB 量级，但**必须与贴图预算同场测**，不单独声称"便宜"。
- IBL 侧另计：环境 cubemap 256²×6 RGBA16F ≈ 3 MiB（mip 链约 4 MiB），预滤波 cubemap 同量级，DFG LUT 128² ≈ 64～128 KiB（RG16F/RGBA16F），SH-L2 144B，GlobalProbe 1KB；与贴图预算相比可忽略，但仍按 §7 成组记录。

### 1.4 推翻判据

- 屏幕播种覆盖率不够，且 bounce 预算提到 4096/帧仍补不上 → 引入投票分配器（查询结构不动）。
- 2060 上 TLAS refit + 流送 BLAS 构建吃掉 3 ms 以上 → 任何世界空间缓存的性价比重算（这是选型之外的共同死线）。
- 步骤 9 之后若走纯路径追踪 → 重新评估 Kajiya 模型；patch 池、rank 语义、L1 SH 存储可直接复用。
- IBL 若在 6GB 显存/帧预算下压不住（预滤波重烘或 cubemap 采样成为瓶颈）→ 降 cubemap 分辨率/降预滤波频率，不推翻结构。

### 1.5 天空与 IBL 选型（2026-11-20 新增）

**结论：程序化天空 + 标准 split-sum IBL（漫反射 SH-L2，高光预滤波 cubemap + DFG LUT）；GlobalProbe 16×L1 作为方向类别 LUT 与 patch 兜底，不作为"比 L2 更准"的默认。**

| 决定 | 选择 | 理由 |
|---|---|---|
| 天空来源 | 物理天空 + 太阳圆盘，`sky_radiance(dir, include_sun)`（2026-11-23 起；立案时为解析渐变，同日被 9.1 追加段替换） | 全动态场景/TOD 不需要 HDR 资产与多套 cubemap 混合；可见天空与 IBL 同源；BRP 可调参，便于验证 |
| 漫反射 | SH-L2（9 系数/通道，折叠余弦卷积） | 标准 IBL；L2 能表达地平线带；27 floats 成本可忽略；与 9.3 的 16×L1 可 A/B |
| 高光 | GGX 预滤波 cubemap + DFG LUT（128²） | 标准 split-sum；8.2 已把 DFG LUT 留给 IBL（[Filament BRDF参考](../../笔记/光照与材质/Filament%20BRDF参考.md) §6） |
| 太阳 | 可见天空画圆盘；IBL 捕获只排圆盘（米氏前向峰保留，HDRP cubemap 烘焙同口径） | 圆盘是高立体角/高频亮斑，进 SH/预滤波会污染低频项，并与直接光高光双计（Unity `_SUNDISK_NONE` 同目的） |
| 更新 | 天空参数 hash 脏标记 + 0.1s 节流；可见天空每帧解析 | TOD 是慢变量；静止零成本；Unity SCGI 的 `GlobalProbe` 也是事件驱动（`SurfaceCache.cs:773-780`） |
| GlobalProbe | 16 桶 × RGBL1（4×4 八面体，32 样本/桶） | 与 patch 同格式兜底；方向类别 LUT；是否默认由 A/B 决定（§3） |
| 不做 | SSR、局部反射探针、云层/大气散射、AO 本体 | 9.2 只做天空环境高光；这些另立或后置 |

**源码依据**：Unity `CubemapRender.cs`（环境 cubemap 渲染、太阳颜色/方向入 hash、`_SUNDISK_NONE`）、`GlobalProbe.compute` / `GlobalProbe.hlsl`（16 桶 × 32 样本、八面体映射）、`SurfaceCache.cs:773-780`（`DistanceFallback` + cubemap hash/intensity 触发）、`Common.hlsl:161-182`（八面体球↔方）。

## 2. 八段拆解

| 段 | 交付范围 | 判定线草案（独立验收） |
|---|---|---|
| 9.1 程序化天空盒 | **实际为物理天空**（三介质单次散射 + Chapman 透射率 + 地面反照率 + 太阳圆盘；立案时为解析渐变，同日被 9.1 追加段替换）；合成 pass 背景分支替换清屏色；天空参数进帧 UBO；`sky_enabled`；BRP `sky_set` | 天空可见、太阳方向与方向光一致；日落红移；`sky_enabled=false` 与步骤 8 逐位一致；天空只改背景，不改受光像素 |
| 9.2 天空盒 IBL | 物理天空捕获成环境 cubemap（256²×6，脏标记 + 节流，排太阳圆盘）；SH-L2 漫反射投影；GGX 预滤波 mip 链 + DFG LUT；不透明/透明环境项替换为 IBL；`ibl_enabled`；`sky_uniform` 常量天空自检开关 | `ibl_enabled=false` 回步骤 8 受光像素；`sky_uniform` 常量天空 `E=πL`；天空方向性可见；金属/光滑面有反射；太阳圆盘不双计 |
| 9.3 GlobalProbe 方向桶 | 16 桶 × RGBL1（4×4 八面体，32 样本/桶）；法线选桶求值；`ibl_mode` L2/16 桶 A/B；为 9.7 Volume 外兜底建格式 | 桶方向映射正确；`sky_uniform` 常量天空每桶 `E=πL`；A/B 有数据；默认模式有依据 |
| 9.4 求交器与射线基建 | 硬件 ray query 主后端 + 可替换接口；BLAS 挂驻留资产、TLAS 每帧从实例表构建；命中点属性重建 | 可对场景求交并返回命中属性；移动/流送后命中跟随、无悬空；feature 关闭走降级路径 |
| 9.5 世界空间表面缓存：patch 池 | Volume + 级联格子 + 16 方向桶 + 定长 ring；屏幕播种/复用/滚动/心跳/整理；rank 语义；motion vectors 前置件 | patch 分布与 G-buffer 吻合；显存恒定；chunk 增删移动后跟随；稳态零增长 |
| 9.6 irradiance 更新 | 每活跃 patch 半球采样；直接光（方向/点光/自发光）与缓存自查询多跳；L1 投影 + 方差引导时域混合；bounce 分配 | 多跳色渗可见；开关灯 lag 如实记录；收敛后射线预算降档；空间滤波 A/B |
| 9.7 屏幕 gather 与最终注入 | 1/4 查表（rank 0）+ 空间/时域滤波 + 去噪 + 双边上采样 + L1 压缩；替换 Volume 内的 IBL 漫反射，体积外保持 9.3 的漫反射编码（同属 IBL 基线） | SCGI 关 = IBL 基线；漏光案例留档；查询不读 rank 1；成本随分辨率不随实例数 |
| 9.8 动态验收、压测与局限对照 | 动态几何/动态光照收敛行为；Volume 边界与兜底；成组压测；已知局限逐项自检 | 静态游历零重传零泄漏；动态无永久残影；压测成组；GPU-driven 闭环不回退 |

### 9.1 程序化天空盒

> **2026-11-23 补记**：本段已收官；渐变模型同日被**物理天空**（A 档单次散射、无 LUT）替换，以下立案原文保留作历史，现行交付/判定线以[9.1 追加计划](9.1-程序化天空盒/施工计划：物理天空（9.1追加）.md)与[段 README](9.1-程序化天空盒/README.md)为准。

- **一句话说明**：用解析天空函数替换清屏色，产出可见的程序化天空与可复用的 `sky_radiance(dir)`；本段不捕获 cubemap、不投影 SH、不改任何材质的环境光。
- **交付**：
  - `sky_radiance(dir, include_sun)`：天顶—地平线—地面渐变（两个 `pow` 控制过渡）+ 太阳圆盘（方向 `dir_to_light`、颜色 `light_color`、角半径/增益可调）+ 可选光晕。
  - 合成 pass 背景分支：`sky_enabled ? sky_radiance(dir, true) * EXPOSURE : CLEAR_COLOR`；背景方向由 `world_from_depth(uv, 0)` 反投影减去 `camera_pos` 得到。
  - 帧 UBO 208→272：`sky_enabled`/`sky_intensity`（116/120）+ `sky_zenith`/`sky_horizon`/`sky_ground`/`sky_sun` 四行（208/224/240/256）。
  - `SkySettings` 资源 + `sky_set`/`sky_stats` BRP；egui 不接。
- **落点**：`shaders/common/sky.wgsl`、`shaders/scene/gbuffer/gbuffer_composite.wgsl`、`src/render/passes/sky/`、`src/render/resident/tables/frame_uniform_table.rs`、`src/remote/features/sky.rs`、`src/driver/host/frame/scene.rs`。
- **判定线草案**：①天空可见且太阳随方向光移动；②`sky_enabled=false` 与步骤 8 背景逐位一致；③天空开只改背景，受光像素不变；④参数即时反映；⑤零 VUID、clippy 全净。

### 9.2 天空盒 IBL

- **一句话说明**：把 9.1 的程序化天空捕获为环境 cubemap，投影 SH-L2 漫反射并预滤波高光，替换不透明与透明的常量环境项；本段不产生世界空间缓存、射线或 SCGI。
- **交付**：
  - 常量天空开关（9.2.0）：帧 UBO 304→336，`sky_uniform` 让 `sky_radiance` 返回常量辐亮度；只为自检，默认关。
  - 环境 cubemap：256²×6 RGBA16F、cube 兼容、9 级 mip；`sky_radiance(dir, false)` 捕获（只排太阳圆盘，米氏前向峰保留）；`sky_hash`（含 100 m 量化海拔）脏标记 + 0.1s 节流；② 段 compute 录制；`ImageBarrier` 加子资源范围以覆盖 9 mip × 6 层。
  - SH-L2：1024 Hammersley 方向投影，9 系数/通道，`A0=π/A1=2π/3/A2=π/4` 折叠；storage buffer 9×vec4（144B）；两条自检（l=0 归一化对 `mean(L)`、`E_sh` 对暴力余弦积分）。
  - 高光：GGX 预滤波 mip 1..8 + 128² DFG LUT（或解析 `EnvBRDFApprox` 兜底）；`prefiltered(R, perceptualRoughness) · (F0·A+B)`。
  - 注入：不透明合成与透明前向都把 `albedo × ambient_color` 替换为 `diffuseColor/π·E(n)·ibl_intensity + specular`；`ibl_enabled=false` 逐位回退。
  - 帧 UBO 304→336：`ibl_enabled`/`ibl_mode`/`ibl_intensity`（304/308/312）+ `sky_uniform`（320）。
  - 新 IBL 描述符集（group 4）：cube + sampler + DFG LUT + SH buffer；cubemap 不走 bindless 2D 表（set0 只收 `texture_2d`）。
  - `ibl_set`/`ibl_stats`；mode 9/10 调试视图（IBL 漫反射/高光贡献）。
- **落点**：`shaders/common/ibl.wgsl`、`shaders/scene/ibl/`、`src/render/passes/ibl/`、两个光照着色 pass、两个管线配方、`src/remote/features/ibl.rs`、`frame_uniform_table.rs`、`core/record/barrier.rs`、`driver/host/init.rs`。
- **判定线草案**：①`sky_enabled=false` + `ibl_enabled=false` 全帧 = 步骤 8；②IBL 隔离只改受光像素；③`sky_uniform` 常量天空 `E=πL` + 两条积分自检；④方向性/高光/透明可见；⑤太阳圆盘不双计（米氏峰允许）；⑥室内漏光记录；⑦零 VUID、clippy 全净。

### 9.3 GlobalProbe 方向桶

- **一句话说明**：从 9.2 的天空 cubemap 生成 16 桶 × RGBL1 的 GlobalProbe，按法线选桶求值，并用 A/B 对照 L2；本段不产生空间缓存、不发射射线、不替代 L2 作为默认（除非数据支持）。
- **交付**：
  - `octahedral.wgsl`：八面体球↔方、`direction_to_bucket_index`、`bucket_index_to_direction`；与 Unity `Common.hlsl:161-182` 同式；9.5/9.7 共用。
  - `global_probe.wgsl`：16 组 × 32 样本的半球 L1，`2π/32` 归一化、`A0/A1` 卷积、折叠；16×4 vec4（1KB）。
  - `ibl_mode`：0 = SH-L2（默认），1 = GlobalProbe；不透明/透明漫反射求值分支。
  - `ibl_set` 的 `diffuse_mode`；`ibl_stats` 16 桶摘要；可选桶可视化调试档。
- **落点**：`shaders/common/octahedral.wgsl`、`shaders/scene/ibl/global_probe.wgsl`、`shaders/common/ibl.wgsl`、`src/render/passes/ibl/`、`src/remote/features/ibl.rs`。
- **判定线草案**：①六轴/随机方向映射正确；②`sky_uniform` 常量天空每桶 `E=πL`；③A/B 读图与接缝记录；④`ibl_mode=1` 可用；⑤零 VUID、clippy 全净。

### 9.4 求交器与射线基建

- **一句话说明**：把"射线怎么打、打到什么"做成与 GI 无关的基建——硬件 ray query 主后端、BLAS/TLAS 生命周期跟着 6.x 驻留走、命中点属性可重建；本段不产生缓存、不消费 GI 样本。
- **交付**：
  - 设备能力探测 + feature 链：`VK_KHR_acceleration_structure` + `VK_KHR_ray_query` + `bufferDeviceAddress`（2060 已实测支持）。不支持时 GI 降级关闭而不是崩溃：feature 门 + 启动日志 + BRP 读数。
  - **BLAS**：归 6.x 驻留资产（mesh 上传时构建、回收时销毁、内容变化时重建）。BVH 几何 LOD 策略见 §3 待决，提案=**最粗可用 LOD**——漫反射 GI 只需低频遮挡，BLAS 内存与构建时间省一个量级，代价是遮挡边界略粗。
  - **TLAS**：每帧实例缓冲由 compute 从常驻实例表生成（实例槽/材质索引进 `gl_InstanceCustomIndex`），`vkCmdBuildAccelerationStructuresKHR`；移动只更新实例缓冲，不重建 BLAS；BLAS 未就绪（流送缺席）的实例不进 TLAS。
  - 射线抽象：Rust 侧只暴露 `RayScene`（构建/更新/查询入口）；WGSL 侧封装 `rayQueryInitializeEXT` / `Proceed` / `Candidate`，返回 `(t, instanceID, primitiveID, barycentrics)`；接口不泄漏硬件类型，保留后端可替换语义。
  - 命中点重建：`instanceID → 实例表 → mesh → 顶点索引 →` 世界位置/法线/UV + 材质索引（+ 自发光参数）。这是 9.6 的取数面。
  - 调试视图：命中距离/命中法线/命中实例 ID、射线计数。
- **落点**：`src/render/ray_tracing/`（新增：`context.rs` / `blas.rs` / `tlas.rs`）、`src/render/resident/assets/`（BLAS 归资产）、`shaders/common/raytracing.wgsl`。
- **前置闸门**：AS/ray query 的 feature 启用与未启用两条路径首跑零 VUID；空场景 TLAS 可构建；光线求交与既有 graphics/compute 提交的 barrier 纪律沿用（同一 graphics 队列）；`bufferDeviceAddress` 与 bindless 表共存。
- **判定线草案**：①可对场景求交并返回命中点属性（调试视图验证）；②移动物体后命中跟随、无陈旧（TLAS 更新正确）；③流送卸载后无悬空实例、不崩溃；④不支持 光线求交的设备/feature 关闭时 GI 走关闭路径；⑤零 VUID、clippy 全净。
- **待决**（§3）：BVH 几何 LOD；alpha 裁切；compute 软件求交排期。

### 9.5 世界空间表面缓存：patch 池

- **一句话说明**：把世界空间切成相机跟随的级联格子，每格 16 个方向桶，每桶一条 patch，patch 池定长环形复用；屏幕每帧反推可见表面刷新 patch 几何、保留辐照度历史；回收靠滚动/心跳/整理三件。本段只建池与生命周期，不出辐照度。
- **前置件（不新开段号，随 9.5 任务面板单列）**：**motion vectors 本体**——`InstanceLedger` prev/curr 双份模型环（8.1 只留附件位）。用途=新 patch 的上一帧辐照度重投影种子。若成本超预期，允许降级为"相机重投影种子"（无物体运动补偿），motion vectors 转 9.6 或外置，并把差异记录在段施工记录里。
- **交付**：
  - **Volume**：相机/焦点跟随、Size、Res、CascadeCount、最细体素；级联偏移按体素量化（snap），避免游移导致 cell 抖动。
  - **数据布局（常驻表）**：cell 表（`CellPatchIndices`：`Res³ × 16 × cascades` 个 `uint` patch 索引；`CellAllocationMarks`：分配仲裁；defrag flip-flop 是 ring 配置读写）、patch 表（位置/法线；3 份 RGBL1 辐照度缓冲；mean/variance/updateCount/rank/heartbeat；reverse cell；打包位）、ring 配置、计数器。
  - **分配**：1/4 分辨率屏幕播种读 G-buffer 深度+法线；4×4 全分辨率偏移 16 帧一轮 + 每帧 permute；**cell 命中 → patch 索引不变、位置/法线刷新、rank/heartbeat 刷新、辐照度历史保留**；新 cell → ring 分配，种子=上一帧低分辨率辐照度按 motion vector 重投影（落在同体素内才接受）；池满 → 分配失败计数，不增长。
  - **回收三件**：Scrolling（出界 cell 释放）、Eviction（240 帧无心跳超时）、Defrag（subgroup 前缀和压缩，每帧 2/4/8/16 槽，双表 flip-flop）。
  - **rank**：0 = 屏幕生成，1 = bounce 生成（9.6 启用）；屏幕查询只读 0（9.7 执行）。
  - 调试视图 + BRP：patch 分布（按级联/桶着色）、cell 占用率、分配失败、rank 分布、池水位。
  - 参数基线：Res 32 / 4 级联 / Size 128 / 16 桶 / 池 65536（§1.3）。
- **落点**：`src/render/resident/tables/surface_cache.rs`、`src/render/passes/gi/allocation.rs`、`shaders/scene/gi/{patch_alloc,scrolling,eviction,defrag}.wgsl`、`shaders/common/gi_cell.wgsl`。
- **前置闸门**：cell 表双表读写 hazard（barrier 纪律）；屏幕播种读 G-buffer 的取数面（**2026-11-24 订正**：深度早已扩 `SAMPLED` usage、合成已在采样，"6.1 已实证深度附件不可采样"是 6.1 立案期的旧结论；真正的门槛是 G-buffer 采样集的 stage 只声明了 `FRAGMENT`，compute 要扩 stage 或另立集——见[9.5 施工计划](9.5-patch池与分配回收/施工计划：patch池与分配回收.md) §0.0/§0.7）；motion vectors 前置件（9.5 已裁决为"相机重投影接口先留、本体降级"，同文 §0.4）。
- **判定线草案**：①patch 分布与 G-buffer 表面吻合（读图）；②池与 cell 表显存恒定、不随运行时长增长；③chunk 增删/移动后 patch 跟随、无残留；④稳态分配/回收计数平衡；⑤大世界档（Size 512 / 6 级联）可持续游历、滚动后旧 cell 释放；⑥零 VUID、clippy 全净。

### 9.6 irradiance 更新

> **2026-11-24 补记**：本段已细化立案，见[施工计划：9.6 irradiance 更新](9.6-irradiance更新/施工计划：irradiance更新与多跳.md)。该文订正本节三处——"材质环补 emissive 因子/贴图槽"**已由既有材质环提供**（只差自发光三角形列表）；"余弦加权半球"改为与 Unity 同款的均匀半球 + 密度归一；patch 级空间滤波与时域后滤波归 9.7。交付范围、判定线、任务拆解以段计划为准。

- **一句话说明**：每个活跃 patch 每帧发少量半球射线，命中点取"直接光 + cell 缓存查询"，投影成 L1 SH 并按方差引导做时域混合——多跳由缓存自查询自然形成；本段不接屏幕。
- **交付**：
  - 每活跃 patch 一发线程（活跃 ring 槽位）；余弦加权半球采样；`QrngKronecker2D` 低差异序列；样本数 1/2/4/8 档 + 预热倍率（新 patch 加速收敛）。
  - **直接光**：方向光（阴影复用 8.3 CSM；per-ray 用低档 PCF、不跑 PCSS 全价——成本收益 9.6 实测钉明）、点/聚光（光源常驻表 + reconnection 采样）、自发光三角形（面积采样 + MIS）。
  - **自发光前置**：材质环补 emissive 因子/贴图槽；自发光三角形列表每帧双缓冲构建（从实例/材质表筛出，CPU 零参与，含面积 CDF）。
  - **间接光**：命中点查 cell 缓存（rank 规则见 §1.2），构成多跳；命中点无 patch → bounce 分配请求（每帧上限，rank 1）；开 bounce 分配时 rank 0→1 降级。
  - L1 SH 投影 + 余弦卷积；mean/variance/updateCount 统计；方差引导时域混合（长滞后 0.995 / 短 0.75~0.95；`updateMax = 32`）。
  - 全局射线预算硬约束：`活跃 patch 数 × 样本数 ≤ 预算`；超限按优先级降采样（可见性/心跳）。
  - 调试视图：每 patch 样本数、射线数、方差、updateCount、rank。
- **落点**：`src/render/passes/gi/estimation.rs`、`shaders/scene/gi/estimation.wgsl`、`shaders/common/{gi_sampling,gi_emissive}.wgsl`、`src/render/resident/tables/`（自发光三角形列表）、材质环扩列。
- **判定线草案**：①多跳色渗可见（红墙/绿顶对照）；②开关灯 lag 量级如实记录；③收敛后射线预算可降档且画面稳定；④自发光贡献开关 A/B；⑤阴影漏光案例记录（CSM 只覆盖主相机视锥，屏幕外 patch 无遮挡→已知漏光源，见 §3）；⑥零 VUID、clippy 全净。

### 9.7 屏幕 gather 与最终注入

- **一句话说明**：在 1/4 分辨率把像素位置换算成 cell、查 rank 0 的 patch，做 patch 空间滤波 + 时域后滤波 + 去噪 + 双边上采样，最后把 Volume 内的漫反射间接替换成缓存结果——这是"缓存被着色消费"的唯一入口；体积外回退 9.2/9.3 的 IBL。
- **交付**：
  - 1/4 查表：深度重建世界位置 → Volume 内 → cell + 法线方向桶 → patch；法线一致性阈值、年龄渐入、Chebyshev 软可见性（可选）；只读 rank 0。
  - patch 空间滤波：jittered taps、权重 `1 − |jitter|/√2`（Unity 同款），可关。
  - 时域后滤波：`lerp(0.9, 0.95)`（Unity 同款，压屏幕侧闪烁）。
  - 去噪 3×3（步长 `{1,2,2,3}`）+ 4 tap 双边上采样。
  - L1 压缩：`l1 × √3/(4×l0) + 0.5` 存 RGBA8，L0 存 RGBA16F；曝光口径=缓存存物理单位，gather 端乘当前曝光（本工程固定曝光，不做 pre-exposure 补偿；将来接自动曝光再补补偿项）。
  - 注入：**Volume 内不透明**用 patch 的漫反射；**Volume 外**回退 9.3 的 GlobalProbe（或 9.2 的 SH-L2，按 9.3 的 A/B 结论）；透明层始终走 9.2 的 IBL，不接 SCGI。
  - 调试视图：屏幕辐照度、方差、rank、查表命中率。
- **落点**：`src/render/passes/gi/filtering.rs` + `screen_gather.rs`；`shaders/scene/gi/{spatial_filter,temporal_filter,denoise,upsample,screen_gather}.wgsl`；`shaders/common/gi_compression.wgsl`。
- **判定线草案**：①SCGI 关 = IBL 基线（硬线）；②漏光案例读图对照（薄墙/双面/桌面）并记录；③屏幕查询不读 rank 1（强制 rank 视图验证）；④成本随分辨率不随实例数；⑤上采样无块状/振铃；⑥零 VUID、clippy 全净。

### 9.8 动态验收、压测与局限对照

- **一句话说明**：把"能跑"变成"可验收"——动态几何/动态光照（含 TOD 天空）下的收敛行为、Volume 边界、成组压测与已知局限逐项自检。
- **交付**：
  - 动态几何（移动/增删/流送进出）与动态光照（开关灯、昼夜/TOD 天空）的收敛与残影检查。
  - Volume 边界与 DistanceFallback（9.3 GlobalProbe）对照。
  - 压测（按路线图统一口径成组）：Sponza / zero_day / 大世界档（Size 512）；实例档；读数=活跃 patch 数、射线数、分配失败、池水位、cell 占用、`gi_ms`、IBL 捕获耗时、显存。
  - 局限自检清单：diffuse-only、透明不参与 SCGI、无 SSR/局部反射、无天空遮挡（室内漏光）、alpha 裁切（首版按不透明）、CSM 覆盖不足、BVH LOD 精度、薄几何漏光、单相机一份缓存。
  - A/B 对照：IBL 开/关、SH-L2 vs GlobalProbe、GI 开/关、质量档、空间滤波开关；Unity 官方 251 场景与 Atrio 只作"质量参照"（不逐像素对比）。
- **判定线草案**：①静态游历零重传零泄漏；②动态场景无永久残影；③压测报告成组（不拿"流畅"截图代替读数）；④局限自检留档；⑤GPU-driven 闭环不回退（draw_calls 维持 5.3 口径）；⑥零 VUID、clippy 全净。

## 3. 立案期头号待决项汇总

| 待决 | 段 | 选项 | 裁决时机 |
|---|---|---|---|
| 天空模型复杂度 | 9.1 | 解析渐变 + 太阳圆盘 / Preetham-Hosek / 加云 | **已裁决（2026-11-23）**：物理天空（三介质单次散射 + Chapman 透射率 + 地面反照率，A 档无 LUT）；渐变模型同日替换，见 [9.1 追加计划](9.1-程序化天空盒/施工计划：物理天空（9.1追加）.md) |
| cubemap 分辨率 | 9.2 | 128² / 256² | 9.2 立案时按高光质量与显存拍板；提案=256² |
| 高光预滤波与 DFG | 9.2 | GGX 预滤波 + LUT / mip 链 + 解析 `EnvBRDFApprox` | 9.2 立案时拍板；提案=LUT 收尾，解析近似可作 9.2 内的过渡 |
| IBL 更新频率 | 9.2 | 每帧 / 0.1s 节流 / 参数增量阈值 | 9.2 实测后钉明；提案=hash + 0.1s |
| GlobalProbe 默认模式 | 9.3 | SH-L2 默认 / 16 桶默认 | 9.3 的 A/B 读图后裁决；不预设"16 桶更准" |
| IBL 的 AO | 9.2/9.8 | 不乘 / 乘 diffuse AO / 单独 specular occlusion | 9.8 局限对照时裁决；提案=9.2 先不乘，记录室内漏光 |
| BVH 几何 LOD | 9.4 | 逐 LOD 变体建 BLAS / 基准 LOD / 最粗可用 LOD（省内存省构建，遮挡略粗——对 diffuse 可接受） | 9.4 立案时按显存账裁决；提案=最粗可用 LOD |
| compute 软件求交 | 9.4 | 同段完成 / 延后为可选项（接口保留后端可替换语义） | 9.4 立案时拍板；提案=延后，不作为步骤完成门槛 |
| alpha 裁切 | 9.4/9.8 | 首版按不透明（Unity 6.7 同款）/ any-hit 里做 alpha 判定（需 UV 与贴图取样） | 9.8 局限自检时定 |
| 自发光首版 | 9.6 | 进首版（Atrio 式验收需要）/ 方向光+点光先行 | **已裁决（2026-11-24）**：进首版，限不透明、非 alpha 裁切、emissive 非零的三角形；列表上限 32768 条留 9.8 复核。见 [9.6 计划](9.6-irradiance更新/施工计划：irradiance更新与多跳.md) §0.3 |
| CSM 与 GI 体积覆盖差 | 9.6 | 扩 CSM 覆盖到 GI Volume（阴影分辨率下降）/ 接受屏幕外无遮挡漏光并记录 | **已裁决（2026-11-24）**：接受屏幕外无遮挡漏光并记录，不扩 CSM 覆盖；漏光案例进 9.8 局限对照，见 [9.6 计划](9.6-irradiance更新/施工计划：irradiance更新与多跳.md) §0.2 与判定线 ⑤ |
| patch 池大小 | 9.5 | 65536 / 2^18 | **已裁决（2026-11-24）**：起步 65536（满足环形取模前提），2^18 留大世界档分配失败率触发，见 [9.5 计划](9.5-patch池与分配回收/施工计划：patch池与分配回收.md) §0.2 |
| 播种分辨率与滤波形态 | 9.5/9.7 | 1/4 固定 / 自适应；空间滤波与去噪默认开或关 | **9.5 半裁（2026-11-24）**：分辨率=1/4 固定 + 4×4 子像素 16 帧轮转（见 [9.5 计划](9.5-patch池与分配回收/施工计划：patch池与分配回收.md) §0.3）；滤波形态归 9.7 立案 |
| Volume 形状与焦点 | 9.5 | 球 / 盒；相机跟随 vs Focus Target | **已裁决（2026-11-24）**：球判据 + 立方存储、焦点=相机位置、Focus Target 不做（见 [9.5 计划](9.5-patch池与分配回收/施工计划：patch池与分配回收.md) §0.1） |

## 4. 与步骤 8 交付物的消费关系

- **G-buffer 三原料**（albedo / normal / 世界位置深度重建）→ 9.5 屏幕播种、9.7 屏幕查表的取数面；9.1 的天空背景、9.2 的 IBL 都复用深度反投影。
- **光照累积 pass 的环境项** → 9.1 加天空背景；9.2 把环境项替换为 IBL；9.7 再把 Volume 内的漫反射替换为 SCGI；`ibl_enabled=false`/`gi_enabled=false` 分别回退到上一层的原文。
- **透明前向的环境项** → 9.2 接 IBL；9.7 不改透明（SCGI 只作用不透明）。
- **深度语义**：与 6.1 反向 Z 对齐；深度附件现为 `DEPTH_STENCIL_ATTACHMENT｜SAMPLED`（**2026-11-24 订正**：旧文"不可采样"是 6.1 立案期结论，Hi-Z 为采样扩了 usage 后失效）→ 播种在 compute 里直接 `textureLoad` 取深度，不必绕 `gbuffer_composite` 通道。
- **CSM/PCSS** → 9.6 直接光遮挡复用；阴影分辨率与级联覆盖的边界见 §3。
- **光源常驻表** → 9.6 点/聚光取数。
- **材质环** → 9.6 补 emissive 因子/贴图槽（扩列沿 5.1.1 解冻清单先例）。
- **motion vectors 附件位** → 9.5 前置件写入内容。
- **固定曝光** → 9.1/9.2/9.7 统一在 pre-exposure 域，末尾乘 `EXPOSURE`；自动曝光的补偿项后置。

## 5. 目录组织

沿用步骤 8 定案的 `render/` 归属，不新造顶层域文件夹——天空/IBL 与 GI 的落点分散在"常驻表 / pass / 求交器"三处既有归属里：

```
src/render/
├── passes/sky/                          # 新增：SkySettings 与天空参数
├── passes/ibl/                          # 新增：环境 cubemap、SH-L2、预滤波、GlobalProbe
├── ray_tracing/                         # 新增：求交器（能力探测与 feature 门、BLAS/TLAS 生命周期；9.4 落地现名，立案时写作 rt/）
├── resident/tables/surface_cache.rs     # 常驻表族新成员：cell 表 + patch 表 + ring 配置
├── passes/gi/                           # GI pass 录制：allocation / estimation / filtering / screen_gather
└── stats/                               # gi_ms、patch 数、射线数、分配失败、IBL 捕获耗时

shaders/common/sky.wgsl                  # 天空函数（9.1）
shaders/common/ibl.wgsl                  # SH-L2 / split-sum / 模式分支（9.2/9.3）
shaders/common/octahedral.wgsl           # 八面体映射与桶索引（9.3/9.5/9.7 共用）
shaders/common/gi_cell.wgsl              # Volume/cell/桶的纯函数（9.5 起，9.7 复用）
shaders/scene/ibl/                       # capture / sh_project / prefilter / brdf_lut / global_probe
shaders/scene/gi/                        # 与 Unity 对照的 patch 池/更新/滤波 pass 一族
shaders/common/gi_*.wgsl                 # SH、cell 地址、采样、压缩等公共片段
```

- 命名沿 `passes/<域>/` + `resident/tables/` 既有惯例；shader 分类沿 `shaders/scene/<pass>/`，公共片段经 build.rs 的 `// #import` 展开。
- 一次只施工一段：9.1 天空 → 9.2 IBL → 9.3 GlobalProbe → 9.4 求交器 → 9.5 建池 → 9.6 出辐照度 → 9.7 接屏幕 → 9.8 收口。**顺序不可倒**：IBL 是 SCGI 的对照基线，求交器是缓存的前置；9.4 与 9.3 可并行，但 9.7 必须等 9.2/9.3/9.5/9.6 全部就位。

## 6. 边界

- **diffuse-only 照抄 SCGI 刀法**：SCGI 只做间接漫反射；9.2 的天空环境高光（split-sum IBL）不是镜面间接，SSR/局部反射探针不主张。
- **IBL 不是 GI**：9.1～9.3 没有遮挡、没有位置变化、没有多跳；室内漏光是已知局限，真正的空间变化由 9.4～9.7 的 Volume 缓存承担。
- **AO 不由 GI 自带**（9.2 的 IBL 也不乘 AO；探针兜底 + SSAO 伪造是官方同款组合，SSAO 不立项）。
- 屏幕锐利阴影仍归步骤 8 的 shadow map——GI 只吸收 patch 粒度的低频遮挡。
- 不做烘焙/lightmap/APV 路线；TAA 不做（GI 的主平滑在 patch 条目内）。
- 光源支持对齐 SCGI 现状：Directional + Point/Spot + 自发光三角形起步；Area Light 不主张（Unity 同款）。
- 不做多相机：一份缓存服务一个主相机；多视口/分屏另案。
- 不引入 Kajiya 的 ReSTIR 蓄水池与 output-sensitive 主分配（除非 §1.4 判据触发）。
- 不做 denoiser 模型/学习式重建；不用 DLSS/FSR 类时域重建替 GI 收敛。
- 不与步骤 7 的内容生成耦合；VT 仍按调研结论不立项（外置条目 6）。
- **求交器后端**：射线抽象保留可替换语义（接口不泄漏 `VK_KHR_ray_query` 类型）；硬件 ray query 为 9.4 主后端，compute 软件求交降为可延后任务，不作为本步完成门槛（取代路线图原"软件求交底线"表述，2026-11-20 选型拍板）。
- **天空/IBL**：不做 HDR 天空盒资产管线、不做 SSR、不做云层与大气透视/体积雾；物理天空的 `sky_radiance` 接口保留以后接多重散射 LUT（9.1 B 档）的余地。

## 7. 步骤级判定线与性能口径

1. **环境项可分层回退**：`sky_enabled=false` + `ibl_enabled=false` = 步骤 8；`ibl_enabled=true` + `gi_enabled=false` = IBL 基线；`gi_enabled=true` = SCGI。每一层的开关 A/B 都要有画面与统计对照。
2. **IBL 能量自检**：`sky_uniform` 常量天空 `E(n)=πL`（9.2.0 的测试开关；物理天空本身没有均匀配置）；两条积分自检（l=0 归一化、`E_sh` 对暴力余弦积分）；天空方向性（上/下/太阳侧）与高光（金属/粗糙度）读图可辨；太阳圆盘不双计（米氏峰允许）。
3. **射线预算按 patch 不按像素**：记录活跃 patch 数、每帧射线数、样本档位、收敛前后的预算变化；成本随 patch 数增长，不随分辨率线性增长（屏幕侧滤波除外，单列）。
4. **显存成组**：环境 cubemap/预滤波/DFG LUT/SH/GlobalProbe、cell 表、patch 池、BLAS/TLAS、屏幕目标分别记录，与贴图预算同场测。
5. **动态行为**：开关灯/移动/增删/流送进出/TOD 天空的收敛与滞后量级如实记录，不承诺固定帧数。
6. **漏光与伪影**：薄墙/双面/桌面/Volume 边界/室内天空漏光案例读图留档；不修就写进局限，不隐去。
7. **不破底座**：GPU-driven 闭环不回退（draw_calls 5.3 口径）、4.x 账面零泄漏、零 VUID、clippy 全净、WM_CLOSE exit 0。

## 8. 对照索引：Unity / Filament 源码 ↔ 本工程

| 源码 | 本工程落点 | 段 |
|---|---|---|
| `CubemapRender.cs`（环境 cubemap 渲染、太阳颜色/方向入 hash、`_SUNDISK_NONE`） | `shaders/scene/ibl/sky_capture.wgsl` + `src/render/passes/ibl/` | 9.1/9.2 |
| `GlobalProbe.compute` / `GlobalProbe.hlsl`（16 桶 × 32 样本、八面体映射） | `global_probe.wgsl` + `octahedral.wgsl` | 9.3 |
| `SurfaceCache.cs:773-780`（`DistanceFallback` + cubemap hash/intensity 触发） | 9.2 的脏标记 + 节流；9.7 的 Volume 外回退 | 9.2/9.7 |
| `Common.hlsl:161-182`（八面体球↔方） | `shaders/common/octahedral.wgsl` | 9.3 |
| `SurfaceCache.cs` `RecordPreparation`（Scrolling → Eviction → Defrag） | `passes/gi/allocation.rs` 录制顺序 | 9.5 |
| `SurfaceCache.cs` `RecordUpdate`（Estimation → Filtering → 可选 PatchAllocation） | `passes/gi/` 录制顺序 | 9.6/9.7 |
| `PatchAllocation.compute` | `patch_alloc.wgsl` | 9.5 |
| `Scrolling.compute` / `Eviction.compute` / `Defrag.compute` | 同名 shader | 9.5 |
| `Estimation.compute` + `PathTracing.hlsl` | `estimation.wgsl` + `gi_sampling.wgsl` | 9.6 |
| `SpatialFiltering.compute` / `TemporalFiltering.compute` | 同名 shader | 9.7 |
| `IrradianceCompression.hlsl` | `gi_compression.wgsl` | 9.7 |
| `ScreenResolveLookup` / `ScreenResolveUpsampling`（URP 侧） | `screen_gather.wgsl` / `upsample.wgsl` | 9.7 |
| `SurfaceCacheWorld.cs`（变更跟踪 / 材质图集 / BVH 同步） | `render/ray_tracing/` + `resident/assets/` | 9.4 |
| Filament `surface_shading` / `DFG`（split-sum、`energyCompensation`） | `ibl.wgsl` + `brdf_lut.wgsl` + 两个光照 pass | 9.2 |

机制细节与数据布局逐个 pass 的源码级整理见[Unity SCGI 实现](../../笔记/光照与材质/Unity%20SCGI%20实现.md)；发布状态、参数面与已知局限见[Unity SCGI 调研](../../笔记/光照与材质/Unity%20SCGI%20调研.md)；谱系定位与选型阶梯见[全局光照谱系调研](../../笔记/光照与材质/全局光照谱系调研.md)；IBL/DFG 的对照基准见[Filament BRDF参考](../../笔记/光照与材质/Filament%20BRDF参考.md)。

### 8.1 执行方式与源码本机位置（2026-11-20 用户拍板）

**执行方式：可以模仿 / 照抄。** 写 9.2、9.4～9.7 时允许对着 Unity、Filament 与 Kajiya 的源码逐行搬运——pass 顺序、数据布局、采样与滤波公式都可以直接照抄，再按本工程的 Rust/WGSL 类型与命名落地；与 6GB 显存、大世界、GPU-driven 冲突的地方按 §1.3 参数基线改，改动记进段施工记录。

| 对象 | 本机位置（2026-11-20 核实） | 主要文件 |
|---|---|---|
| Unity core：SCGI 主体 | `F:\Program Files\Unity\7000.0.0a9\Editor\Data\Resources\PackageManager\BuiltInPackages\com.unity.render-pipelines.core\Runtime\Lighting\SurfaceCache\` | `SurfaceCache.cs`（Volume/质量档/录制顺序）、`SurfaceCacheWorld.cs`（场景跟踪/BVH/材质池）、`PatchAllocation.compute`、`Scrolling.compute`、`Eviction.compute`、`Defrag.compute`、`Estimation.compute`、`SpatialFiltering.compute`、`TemporalFiltering.compute`、`GlobalProbe.compute`、`EmissiveTriangleAddition.compute` / `EmissiveTriangleRemoval.compute`、`PunctualLightSampling.compute`、`PathTracing.hlsl`、`PatchUtil.hlsl`、`RingBuffer.hlsl`、`IrradianceCompression.hlsl`、`Common.hlsl`、`VectorLogic.hlsl`、`EmissiveTriangles.hlsl`、`PunctualLights.hlsl`、`GlobalProbe.hlsl`、`PatchAllocationRequest.hlsl` |
| Unity 环境 cubemap | `...\com.unity.render-pipelines.core\Runtime\PathTracing\Environment\CubemapRender.cs` | 六面渲染、`_SUNDISK_NONE`、太阳颜色/方向 hash |
| Unity URP：屏幕侧与宿主集成 | `...\com.unity.render-pipelines.universal\Runtime\RendererFeatures\SurfaceCacheGIRendererFeature\` | `SurfaceCacheGIRendererFeature.cs`（128² 环境 cubemap、`RenderSettings.sun`）、`SurfaceCacheWorldAdapter.cs`、`PatchAllocation.compute`（URP 版）、`FlatNormalResolution.compute`、`ScreenResolveLookup.compute`、`ScreenResolveDenoising.compute`、`ScreenResolveUpsampling.compute`、`Debug.compute` |
| Unity 参数面 | `...\com.unity.render-pipelines.universal\Runtime\Overrides\SurfaceCacheGIVolumeOverride.cs` | 质量档、体积与开关参数定义 |
| Unity 实跑对照工程 | `F:\okzkx\TestSCGI`、`F:\okzkx\TestUnity67`、`F:\okzkx\Unity-Atrio-Demo` | 本机实跑、参数对照与画面参照（Atrio 为官方无 lightmap 演示） |
| Filament：IBL/DFG 对照 | `F:\okzkx\filament` | `shaders/src/surface_lighting.fs`、`surface_light_directional.fs`、`surface_light_punctual.fs`；DFG/prefilter 口径见[Filament BRDF参考](../../笔记/光照与材质/Filament%20BRDF参考.md) |
| Kajiya：irradiance cache | **本机未检出**（2026-11-20）；执行前 clone 到 `F:\okzkx\kajiya`，下面按仓库相对路径引用 | `docs/gi-overview.md`（机制总览）、`crates/lib/kajiya/src/renderers/ircache.rs`（cascade/pool/投票/rank 的 Rust 侧）、`assets/shaders/ircache/`（`ircache_grid.hlsl`、`scroll_cascades.hlsl`、`trace_irradiance.rgen.hlsl`、`trace_accessibility.rgen.hlsl`、`ircache_validate.rgen.hlsl`、`sum_up_irradiance.hlsl`、`lookup.hlsl`、`age_ircache_entries.hlsl`、`ircache_compact_entries.hlsl` 等）、`assets/shaders/rtdgi/`（ReSTIR 侧，只作对照、不搬） |

**Kajiya 检出命令（执行前准备）**：`git clone --depth 1 https://github.com/EmbarkStudios/kajiya F:\okzkx\kajiya`。

**许可边界**：Unity 源码是 Unity Companion License——照抄用于本机学习与对照没问题，但不得随公开仓库分发；公开前把照抄段落替换为自写实现。Kajiya 是 Apache-2.0 / MIT 双许可（上游 README 明示），照抄段落保留版权与许可声明即可。

## 9. 开工前置

- 步骤 8 已收官（8.1～8.6）；步骤 5/6 的步骤级验收归档欠账不阻塞本步（沿用路线图当前位置）。
- F25/F26 挂账不阻塞本步（[待修复清单](../待修复清单/待修复清单.md)）。
- 设备能力：本机 RTX 2060 已核实 `VK_KHR_acceleration_structure` / `VK_KHR_ray_query` / `VK_KHR_ray_tracing_pipeline` / `VK_KHR_ray_tracing_position_fetch`。**9.1～9.3 不依赖 光线求交**；9.4 起才要求。
- 验收场：Sponza（室内基座/漏光）、zero_day（室外天空与规模）、DamagedHelmet（金属/粗糙度高光）、大世界档（Size 512）；Unity 251 / Atrio 仅作质量参照。
- 调研底座：[Unity SCGI 调研](../../笔记/光照与材质/Unity%20SCGI%20调研.md)、[Unity SCGI 实现](../../笔记/光照与材质/Unity%20SCGI%20实现.md)、[全局光照谱系调研](../../笔记/光照与材质/全局光照谱系调研.md)、[Filament BRDF参考](../../笔记/光照与材质/Filament%20BRDF参考.md)、[SH 与光照探针](../../笔记/光照与材质/SH与光照探针.md)。
