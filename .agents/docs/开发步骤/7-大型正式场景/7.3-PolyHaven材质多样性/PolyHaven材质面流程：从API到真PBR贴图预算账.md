# Poly Haven 材质面流程：从 API 到真 PBR 贴图预算账（7.3）

本篇面向下一个读代码的人，走读 7.3 的资产获取通道与贴图字节预算列实测。验收结论在[施工记录](7.3.1-7.3.4-PolyHaven材质多样性施工记录：获取脚本、贴图字节预算列与真PBR读图.md)。

## 全景：一句话版

`fetch_polyhaven.py` 从 Poly Haven API 拼出四件套下载（gltf+bin+textures）→ `load_world` 逐件进场（前件整树回收）→ 账面差值对照 → `fine_grained` 开档压贴图字节预算列。本段零渲染机制改动——它喂的是 6.4 第二列第一次真实负载，和 8.2 的材质供料清单。

## 新文件分工表

| 文件 | 一层职责 |
|---|---|
| `tools/fetch_polyhaven.py` | 资产获取：API files → gltf 包 + bin + textures 补链，幂等落 `assets/polyhaven/` |
| `tools/scene_polyhaven.py` | 一键压测：逐件进场/差值对照/预算列 A/B/读图取证 |

## 逐时刻走读

**① 取材**：`files/<Asset>` 响应内 `gltf/<res>/gltf.url` 是打包 gltf；**包非自含**——buffer URI 走 gltf 同目录，贴图 URI `textures/X.jpg` 基名映射 `Models/jpg/<res>/<Asset>/X`。选件以"贴图槽全 + 材质互补"为纲（布/瓷/黄铜/木雕）。

**② 进场与换场**：`load_world` 逐件 + `world.despawn_entity` 整树回收前件——多场景共存口的另一用法：**独占场景**。坑两枚：同资产重载被 AssetServer 去重（pool 条目不随实体涨），despawn 传播滞后于秒级读数——段间基线现读。

**③ 账面**：Δrows 逐件对照 gltf 解析清单；贴图字节走 `residency_stats.fine.used_image_bytes`（**协议字段住 fine 嵌套对象**，顶层没有）。

**④ 预算列**：`residency_set {fine_grained:true, budget_image_bytes}` 开档 → 贴图整资产淘汰收敛到预算（used_image 钉在 budget 上）→ 关闭沿全行置脏回全量。fallback 白图保底语义沿用 6.4（本段淘汰后远端对象走白图，读图两态由 ① 的独占场景截图背书）。

## 失败路径

- **gltf 引用缺失**：贴图/bin 没下齐 → 材质槽永不到货（7.1 jpeg 钉的姐妹坑），采集核验凑不齐 warn。fetch 脚本按 gltf URI 逐条补链防漏。
- **预算列误读**：字段在 `fine` 下；`get('used_image_bytes', 0)` 永远 0——7.2 的"未填充"误报就是这一类。
- **UA 403**：dl.polyhaven.org 对部分 UA 敏感，curl 带自定义 UA 兜底。

## 代码改动简要解析（feat f88bba5c0，`git show --stat` 实数）

| 文件 | ± | 改动要点 |
|---|---|---|
| `tools/fetch_polyhaven.py` | +126 | 获取脚本（API 拼装/幂等标记/curl 兜底/makedirs） |
| `tools/scene_polyhaven.py` | +293 | 一键压测（换场基线现读/差值断言/预算列 A/B/逐件特写取证） |

取舍：不改 residency_stats 协议（字段已在，只是脚本读错层）；不加 AO/发光槽消费（ARM 打包贴图的 AO 半边归 8.2 材质面一起收）。
