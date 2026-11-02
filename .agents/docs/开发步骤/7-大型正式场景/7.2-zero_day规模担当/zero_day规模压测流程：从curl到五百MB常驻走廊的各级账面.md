# zero_day 规模压测流程：从 curl 到五百 MB 常驻走廊的各级账面（7.2）

本篇面向下一个读代码的人，走读 7.2 的多场景进场与规模压测链路。验收结论在[施工记录](7.2.1-7.2.6-zero_day规模压测施工记录：BRPload_world、五百MB进场与预算流送.md)。

## 全景：一句话版

BRP `load_world` 发整场景进场请求 → bevy glTF loader 吃 500MB 自含 .glb → 7159 个 primitive 反射展开入账 → 单批 1.3GB 上传过 GPU → 预算 1/4 下走廊流送游历。7.2 证明的是：**步骤 3~6 的链路在 31 倍于 Sponza 的规模上不用改一行机制——新增的只有"多场景共存口"和压测脚本的口径纪律。**

## 新文件分工表

| 文件 | 一层职责 |
|---|---|
| `ash_renderer/src/remote/assets.rs`（扩展） | `load_world_cmd`：整场景动态进场的协议薄壳，进场机制零新造 |
| `tools/brp.py`（扩展） | `load-world` 子命令透传 |
| `tools/scene_zero_day.py` | 一键压测：glb 解析 → 进场等待（相机卫兵）→ 清单对照 → 规模读数 → 预算流送 → 6.5/6.6 复验 → A/B 恢复 → 零 VUID |

## 逐时刻走读

**① 取材**：curl release glb → `assets/zero_day/`（exclude 层处理）。glb = 12 字节头 + JSON chunk + 二进制 chunk，脚本运行时解析出 primitive/材质/贴图清单与 AABB——期望值与站位都从资产现算。

**② 进场**：`load_world {path}` 与 Startup 的 `load_sponza` 走同一条链（load → WorldAssetRoot → world_instance_spawner 反射展开）——多场景共存 = 多个 WorldAssetRoot 并存，账本各自行，驻留账统一按 AssetId 聚合。

**③ 展开**：6 个顶层节点（ALL_INSTRUMENTS/DOOR/DOOR_1/ROOM/SYM_INSTRUMENTS_R + 内嵌相机节点）→ 7159 个 primitive 实体。**内嵌相机是雷**：它让 World 出现第二台 Camera，所有 `single()` 读法（residency_stats/draw/取景 VP）全崩——压测脚本轮询中遇 BRP 拒答就清外（保留最早实体），机制卫兵归 7.4。

**④ 入账与上传**：账本 7159 行增量 flush → 上传批次 #135 单批 mesh 7159 个（221.9MB 顶点 + 71.9MB 索引）+ 贴图 396 张 2.08GB——单批 1.3GB 过账，50s 排空。池容量顶点 296.7MB/索引 96.5MB，实占 71%/74% 未触顶。

**⑤ 收敛与流送**：全常驻基线（enabled=false）下稳态零重传；预算 1/4 + 半径全覆盖时 ejection 持续消化（56006 累计），used 钉在 73.4MB ≤ budget，未驻留行 DrawList 暂缓（命令 3159 ≤ 7262，零幽灵）；关闭沿回全量。

**⑥ 复验**：无变体资产上 floor_anchor = 保底即基准（锚无可迁、读数零回退）；gap_mode=Merged 请求有界无当帧等待——6.5/6.6 的"真实资产复验"尾巴清账。

## 失败路径

- **多相机**：任何进场场景带 camera 节点都会踩；脚本层清理是压测代偿，机制卫兵（"宿主唯一相机"）7.4 立账。
- **清单口径**：断言永远用基线差值；贴图实载 ≥ JSON 计数（texture transform 复用展开），等号断言会误报。
- **workspace 卫生**：tools/ 下跑 py_compile 会造 `__pycache__/`，被根 Cargo.toml `tools/*` member glob 收编导致 cargo 全线找不到文件——已 exclude 收口。

## 代码改动简要解析（feat e523d3078，`git show --stat` 实数）

| 文件 | ± | 改动要点 |
|---|---|---|
| `remote/assets.rs` | +31 | load_world_cmd：协议薄壳 + AssetPath<'static> clone 钉 + 异步语义注释 |
| `remote/mod.rs` | +2/-1 | 方法注册 + use 导入 |
| `tools/brp.py` | +8 | load-world 子命令（照 load_mesh 形状） |
| `tools/scene_zero_day.py` | +404 | 一键压测（解析/卫兵轮询/差值断言/五段采证） |

取舍：内嵌相机清理解堵放脚本层不进机制层——7.2 边界"不做机制扩展"；卫兵是一行语义决定（谁拥有取景权），归 7.4 与其他真实资产缺陷一起立账。
