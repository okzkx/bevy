# Sponza 场景基座流程：从进场请求到常驻验收场的各级账面（7.1）

本篇面向下一个读代码的人，走读 7.1 之后的场景进场链路：一个 Sponza glTF 从 Startup 的 load 请求，到 103 个可绘 primitive 常驻、被一键压测脚本验收的全过程。验收结论在[施工记录](7.1.1-7.1.6-Sponza场景基座施工记录：进场切换、jpeg特征、中庭机位与一键压测.md)，这里只讲流程。

## 全景：一句话版

Startup 发一个 load 请求 → bevy glTF loader 拉齐 bin+69 张 jpg 依赖 → WorldAssetRoot 到货反射展开 103 个 primitive 实体 → 对象账本入行、上传批次上 GPU → bindless 双表/驻留账逐级收数 → `tools/scene_sponza.py` 从 BRP 读数采证。7.1 本身没引入新机制，它证明的是：**这条 3.x~6.x 的老链路吃下真实资产包（非均匀节点缩放、69 张 jpg、25 材质、1 万倍尺寸跨度）不需要任何机制改动——只缺一个 jpeg feature。**

## 新文件分工表

| 文件 | 一层职责 |
|---|---|
| `ash_renderer/src/scene/content/world_asset.rs` | 进场请求与到货报告：SPONZA 路径常量、期望值（103/25/71）钉在 content 层随场景走 |
| `ash_renderer/src/scene/content/collect_report.rs` | 采集核验：账本行 vs 实体 vs 容器三样俱全 + 传播契约（本段第一次守非恒等父链） |
| `ash_renderer/src/scene/content/camera.rs` | 中庭机位常量：pos/target 反解自 BRP 实拍选型的球坐标 |
| `ash_renderer/src/scene/content/mod.rs` | SceneEntryPlugin 组清单：换场景 = 增删组 |
| `ash_renderer/Cargo.toml` | bevy `jpeg` feature：真实资产包贴图的 loader 准入 |
| `tools/scene_sponza.py` | 一键压测：等加载 → 清单对照 → 环游零重传 → 预算负例 → A/B 恢复 → 零 VUID |

## 逐时刻走读

**① 进场（Startup）**：`load_sponza` 调 `server.load(GltfAssetLabel::Scene(0).from_asset("sponza/glTF/Sponza.gltf"))` 并 spawn `WorldAssetRoot`——只发请求不等待。AssetServer 依赖链：Sponza.gltf → Sponza.bin（几何）→ 69 张 jpg（材质贴图，**经 jpeg feature 门**）。

**② 展开（Update，Changed 触发）**：WorldAssetRoot 组件到货，bevy_world_serialization 的 `world_instance_spawner` 把实体树反射展开：GltfNode（scale 0.008）→ 103 个 primitive 子实体，各持 `Mesh3d` + `MeshMaterial3d<StandardMaterial>`（25 份材质按 glTF 材质去重）。

**③ 入账与上传（账本 → 上传批次）**：`update_ledger`（PostUpdate 锚点）把 103 行写进 InstanceLedger → 4.1 增量 flush 发现全部脏行 → 上传批次 #3：mesh 103 个 9.31MB + 贴图逐张异步（362.8MB 总量，几分钟内排空）→ bindless 双表：纹理槽 71/1024、模型环/材质环照常。

**④ 收敛（稳态）**：驻留账 rows 103、resident_meshes 103、resident_images 69（**口径：只数文件贴图；内置 2 张白图不进驻留账**，与容器 Image 71 是两把尺）；脏行 0、requested 0——全常驻基线（ResidencySettings 默认 enabled=false）。

**⑤ 验收（脚本驱动）**：`scene_sponza.py` 五段采证：①清单对照（rows/resident/贴图张数 vs 解析清单）②五站环游零重传（站 0 截图）③预算负例（1/3 预算收敛 + 零幽灵）④A/B 关闭沿恢复 ⑤宿主日志零 VUID/panic。判定出口 = 退出码。

## 失败路径

- **贴图加载失败**（feature 缺/文件缺）：材质 Handle 永不到货，到货报告 Image 计数对不上期望值、采集核验"三样俱全"凑不齐 10s 后 warn——画面静默白图。7.1 的 jpeg 首钉就住在这一格。
- **10s 未到货**：进场报告/采集核验各 warn 一次（Tier①，根因在资产侧），帧循环照常——渲染器不因内容缺席停摆。
- **预算负例中的超支**：半径全覆盖 + 1/3 预算时 ejection 触及请求区，`over_budget` 如实报 true（本段实测 false，因为 9.3MB 的 1/3 装得下半径内最近身份集），不是失败信号。

## 代码改动简要解析（feat 2f2d5c585，`git show --stat` 实数）

| 文件 | ± | 改动要点 |
|---|---|---|
| `ash_renderer/Cargo.toml` | +4/-1 | bevy features 加 `jpeg`，注释钉"invalid image extension 静默回退"的因 |
| `scene/content/world_asset.rs` | +9/-6 | FLIGHT_HELMET→SPONZA、load_flight_helmet→load_sponza、到货报告期望值 6/6/6/17→103/103/25/71 |
| `scene/content/collect_report.rs` | +4/-4 | 期望值与传播契约注释（FlightHelmet 全恒等→Sponza scale 0.008 非恒等） |
| `scene/content/mod.rs` | +1/-1 | 组清单换 load_sponza |
| `scene/content/camera.rs` | +7/-5 | 取景常量换中庭机位（含选型出处注释），日志文案同步 |
| `tools/scene_sponza.py` | +291 | 新增一键压测（wait_scene 等排空/预算现算 1/3/站位绕中庭/截图取证） |

取舍：不加"期望值随场景资源携带"的通用化（待决 1 落定）——content 层本来就是"换场景就改这里"的约定位置，通用化是 content 层重构，超出本段边界。
