# 施工计划：zero_day 规模压测拆解（7.2）

**一句话说明**：本段把 500MB 级 zero_day（Beeple 科幻走廊，规模担当）灌进加载链与流送机制，实测大文件加载、显存账目与预算下持续游历，并清 6.5/6.6 立案时留的"真实资产场景复验"尾巴；本段只做规模压测与机制复验，不主张任何渲染画质特性（归步骤 8/9），不做机制本体扩展（缺陷走 7.4）。

立案日期：2026-10-20（随步骤 7 拆解立案；施工用 `/plan-execute` 开工）。**状态：✅ 施工收官（2026-10-20 同日施工，判定线五条全过，见[施工记录](7.2.1-7.2.6-zero_day规模压测施工记录：BRPload_world、五百MB进场与预算流送.md)）**。上游拍板见[路线图步骤 7 节](../../学习目标实现步骤.md)；Sponza 基线见[7.1 施工记录](../7.1-Sponza场景基座/7.1.1-7.1.6-Sponza场景基座施工记录：进场切换、jpeg特征、中庭机位与一键压测.md)。

## 定位与现状核实

zero_day 是 Beeple 科幻走廊（NVIDIA ORCA），上游 `examples/large_scenes/zero_day/` 以运行时 https asset source 加载。资产形态核实（属实，2026-10-20）：

1. **发布物**：`bevyengine/bevy_asset_files` release `zero-day-1.0`，每场景一个自含 .glb——`zero_day_measure_one.glb`（500.1MB，实测 302→200 可直下）、`zero_day_measure_seven.glb`/`_colored_lights.glb`（515.8MB）。本段取 **measure_one**（默认场景）。
2. **许可**：CC BY 4.0（上游 README 钉明，转换文件自原作修改）。
3. **上游加载形状**：`https` asset source + `web_asset_cache`（缓存于仓根 `.web-asset-cache`）；本项目加载链是 file 资产根——**取材路径 = 施工时 curl 直下 measure_one.glb 到 `assets/zero_day/`**（一次性，约 500MB；落位进 `.git/info/exclude` 与 sponza 同款处理）。
4. **glb 加载形状**：bevy_gltf 对 .glb 与 .gltf 同链（`GltfAssetLabel::Scene(0).from_asset` 不分容器）——施工时以实载钉明；glb 自含无外部依赖，不存在 7.1 的"依赖文件缺失"失败面。
5. **内容清单**：primitive/材质/贴图数施工时解析 glb JSON chunk 实测钉死（.glb = 12 字节头 + JSON chunk，python 可直读），期望值不预写。

与 Sponza 基线的分工：Sponza（7.1）验证加载链/账面**正确性**（14MB 全常驻无压力）；zero_day（本段）验证**规模**——mesh 驻留字节量级、贴图字节量级、预算淘汰在真实大资产上的行为，及 6.5 保底/6.6 缺口请求的真实资产复验。

## 前置施工件（7.2.1，服务 7.2/7.3/7.4）

**BRP `load_world` 方法族**：参数 `{path}`（如 `zero_day/zero_day_measure_one.glb`、`models/FlightHelmet/FlightHelmet.gltf`）——`server.load(GltfAssetLabel::Scene(0).from_asset(path))` + spawn `WorldAssetRoot`。理由：BRP 优先定例（可复用/外层不污染 lowlevel），本段与 7.3、7.4 全部受益；多场景共存不再依赖"改 Startup 常量"（Sponza 保持 Startup 默认进场，zero_day/后续场景经 BRP 动态进场）。落位 `remote/`，照 load_mesh 等既有方法族形状。

## 任务拆解

| 任务 | 内容 | 交付 |
|---|---|---|
| 7.2.1 BRP load_world 前置件 | 见上；含失败路径（坏路径 → BrpError） | 方法族入库，brp.py 透传 |
| 7.2.2 资产获取与清单钉死 | curl measure_one.glb → `assets/zero_day/`；解析 glb JSON chunk 得 primitive/材质/贴图清单（期望值依据） | 资产就位 + 清单入施工记录 |
| 7.2.3 大文件加载与账面实测 | BRP load_world 进场；到货/采集报告 vs glb 清单对照；mesh/贴图驻留字节、加载耗时、GPU 读数成组记录（对照 Sponza 基线：9.3MB/362.8MB） | 规模基线读数 |
| 7.2.4 预算流送压测 | 走廊五站推进（set_camera 序列）；预算 = 全常驻实占现算（1/3~1/4 档，scene_stream 自适应口径）；游历全程 used ≤ budget、无悬空、命令环正常；关闭沿恢复 A/B | 超预算持续游历判定 |
| 7.2.5 6.5/6.6 真实资产复验 | floor_anchor 开档（高档缺席仍画保底，读图两态）；gap_mode=Merged（缺口合并回流、无当帧等待、恒空负例）——复用 6.5/6.6 判定线口径 | 真实资产复验结论 |
| 7.2.6 一键压测与收官 | tools/scene_zero_day.py（启动→load_world→机位→采证→判定）；施工记录+流程导读+三处状态 | 脚本入库 + 文档收官 |

## 判定线草案

1. **加载零失败零 panic**：measure_one.glb 进场到货，到货/采集报告与 glb 解析清单逐项一致；exit 0。
2. **规模账面成组记录**：mesh 驻留字节/贴图驻留字节/加载耗时/GPU 帧时间实测记录，对照 Sponza 基线（对照"应可实测验证"口径，不预设可达帧率）。
3. **预算下持续游历**：走廊环游全程 used ≤ budget、rows 不掉、command_count ≤ 在场行（零幽灵）、关闭沿回全量。
4. **6.5/6.6 复验**：floor_anchor 开=高档缺席仍有画面（读图两态）；gap_mode=Merged=请求有界去重、无当帧等待、缺口回流。
5. **工程闸门**：clippy 全净、库测试全过、零 VUID、exit 0；scene_zero_day.py 全过。

## 待决项（开工时落定）

1. .glb 经 `GltfAssetLabel::Scene(0)` 加载的兼容性（预期成立，实载钉明）。
2. 预算档位与走廊站位坐标（解析 glb 场景 AABB 后定，机位不拍脑袋）。
3. 500MB 加载的内存峰值与上传排空时长（实测记录；若 flush 逐行上传在大资产上行为异常，属机制缺陷→7.4 立账，本段如实记录）。
4. 贴图字节预算（6.4 第二列）是否在本段开档压测——倾向开（zero_day 贴图量大，正是该列的主场），量级施工时定。

## 边界

- 不主张渲染画质特性（zero_day 的灯光/GI 表现归步骤 9 灵感源，本段只看"画得对、账得住"）。
- 不做剔除/LOD/流送机制扩展；大资产暴露的机制缺陷走 7.4 缺陷轮单独立账。
- 不换 Startup 默认场景（Sponza 保持基座位），多场景共存走 BRP load_world。
- 材质多样性归 7.3 Poly Haven；页级 VT 仍挂[外置拓展条目 6](../../外置拓展需求.md)。

## 底座指向

- 上游例子取材机制：`examples/large_scenes/zero_day/`（BASE_URL/许可/README）
- 驻留管理器口径：6.3/6.4 段目录；保底与缺口：6.5/6.6 段 README 与判定线
- 压测脚本样板：tools/scene_stream.py（预算自适应/环游采证/close_host）、tools/scene_sponza.py（wait_scene 排空等待/截图取证）
- BRP 方法族落位定例：`remote/` 壳 + 测试规范 §1 扩建优先级
