# 7.3 Poly Haven 材质多样性补充（M6）

对应[步骤 7 README](../README.md)与[路线图步骤 7 节](../../学习目标实现步骤.md)。

**目的**：以 CC0 单件资产的真 PBR 贴图面补齐材质多样性，为 8.2 Filament BRDF 验收供料，顺带实测 6.4 贴图字节预算列。

**一句话说明**：本段以 Poly Haven 3~5 件全贴图槽单件资产（diffuse/normal/metallic/roughness）走既有加载链与 BRP load_world 逐件进场，实测贴图字节预算列并产出 8.2 供料清单；不主张 BRDF 本体（归 8.2），不压规模（归 7.2）。

**当前状态：✅ 施工收官（2026-10-20 当日立案当日施工，判定线五条全过、scene_polyhaven 8/8），[施工记录](7.3.1-7.3.4-PolyHaven材质多样性施工记录：获取脚本、贴图字节预算列与真PBR读图.md)。**

## 任务面板

| 任务 | 内容 | 状态 |
|---|---|---|
| 7.3.1 | 资产获取脚本（tools/fetch_polyhaven.py，API→gltf 包+贴图补链） | ✅ |
| 7.3.2 | 逐件接入与账面（Δrows/材质/贴图差值对照） | ✅ |
| 7.3.3 | 贴图字节预算列实测（budget_image_bytes 档 + 白图保底两态） | ✅ |
| 7.3.4 | 真 PBR 读图 + 材质多样性清单 + 收官同步 | ✅ |

判定线五条全过（逐件清单对照/预算列收敛 324.4MB 回 648.9MB/真 PBR 读图/8.2 供料清单四件/工程闸门）。首钉四枚：BRP 贴图字节字段住 fine 嵌套/同资产重载去重/换场 despawn 滞后现读基线/fetch makedirs。

## 材料清单

- [施工计划：PolyHaven材质多样性拆解](施工计划：PolyHaven材质多样性拆解.md)——2026-10-20 立案：下载通道实测（api.polyhaven.com files 端点 → dl.polyhaven.org 打包 gltf + 分离贴图 URL，全站 521 模型 CC0）、任务四项、判定线草案五条、待决四项。
- [7.3.1-7.3.4-PolyHaven材质多样性施工记录](7.3.1-7.3.4-PolyHaven材质多样性施工记录：获取脚本、贴图字节预算列与真PBR读图.md)——2026-10-20 收官：判定线五条全过（scene_polyhaven 8/8）；首钉四枚（fine 嵌套字段/重载去重/换场滞后/makedirs）；待决四项落定，8.2 供料清单四件。
- [PolyHaven材质面流程篇](PolyHaven材质面流程：从API到真PBR贴图预算账.md)——流程导读：取材→进场换场→账面→预算列逐时刻走读 + 代码改动解析。
