# 施工计划：Poly Haven 材质多样性补充拆解（7.3）

**一句话说明**：本段以 Poly Haven CC0 单件资产（真 PBR 贴图面：diffuse/normal/metallic/roughness 全槽）补齐步骤 7 的材质多样性，为步骤 8.2 Filament BRDF 验收供料，并顺带实测 6.4 贴图字节预算列（7.2 因 zero_day 无 LOD 变体未能开档）；本段不主张渲染画质特性（BRDF 本体归 8.2），不压规模（归 7.2）。

立案日期：2026-10-20。上游拍板见[路线图步骤 7 节](../../学习目标实现步骤.md)；基线见 [7.1 记录](../7.1-Sponza场景基座/7.1.1-7.1.6-Sponza场景基座施工记录：进场切换、jpeg特征、中庭机位与一键压测.md)/[7.2 记录](../7.2-zero_day规模担当/7.2.1-7.2.6-zero_day规模压测施工记录：BRPload_world、五百MB进场与预算流送.md)。

## 定位与现状核实

**下载通道核实（属实，2026-10-20 实测 API）**：

1. 资产目录：`api.polyhaven.com/assets?type=models`（全站 521 个模型，CC0）；单资产文件表：`api.polyhaven.com/files/<Asset>`。
2. **打包 gltf**：响应内 `gltf/<res>/gltf.url` = `https://dl.polyhaven.org/file/ph-assets/Models/gltf/<res>/<Asset>_<res>.gltf`（如 `ArmChair_01_2k.gltf`）——是否内嵌 bin/贴图（自含 .gltf+外部文件混合形状）施工时下载一枚实拆钉明。
3. **贴图分离通道**：`Diffuse/nor_gl/Metal/Rough（+ARM 打包）` 各分辨率 jpg/png URL 全量可得——gltf 包内贴图缺失时按此补链。
4. 命名钉：目录键大小写混杂（`ArmChair_01` vs 文件 `Armchair_01_diff_2k.jpg`），下载器按 API 返回 URL 原样取，不做本地大小写推断。

**选件标准（施工时定，预算 3~5 件）**：贴图槽全（Diffuse+normal+metallic+roughness）、单件小（2k 档、几 MB 级）、材质类型互补（布艺/金属/木/陶或石）。候选池：ArmChair_01（布+木）、GothicCabinet_01（木）、再从 521 池选 1~2 件金属/陶器（API 贴图槽元数据筛）。

**基建现状**：BRP `load_world`（7.2.1）已入库——逐件进场零新机制；6.4 贴图字节预算列（`budget_image_bytes`，fine_grained 开档填充）机制已收官——本段是它第一次吃真实大贴图（Poly Haven 单件 2k/4k 贴图远大于 FlightHelmet）。

## 任务拆解

| 任务 | 内容 | 交付 |
|---|---|---|
| 7.3.1 资产获取脚本 | `tools/fetch_polyhaven.py`：API files → gltf 包（+缺失贴图分离补链）→ `assets/polyhaven/<Asset>/`；选件清单钉死入脚本 | 获取脚本 + 资产就位 |
| 7.3.2 逐件接入与账面 | BRP load_world 逐件进场；Δrows/材质/贴图与各 gltf 解析清单对照（7.2 基线差值口径沿用）；材质参数读数（collect_report 已带 rgba/贴图槽计数） | 清单对照记录 |
| 7.3.3 贴图字节预算列实测 | `residency_set` 开 fine_grained + budget_image_bytes 档（量级按实占现算 1/2 档）→ 贴图淘汰/fallback 白图回退/恢复 A/B；无悬空 | 6.4 第二列真实压力实测 |
| 7.3.4 真 PBR 读图与收官 | 材质各分项视觉可辨（normal 起伏/roughness 光斑差异/metallic 反照）；材质多样性清单成组记录（8.2 供料清单）；一键脚本 + 施工记录 + 三处状态 | 读图证据 + 文档收官 |

## 判定线草案

1. **逐件加载零失败**：3~5 件全部到货，Δrows/Δ材质/Δ贴图与 gltf 解析清单一致（差值口径）；exit 0。
2. **贴图字节预算列**：budget_image_bytes 档下 used_image ≤ budget、fallback 白图保底读图两态、关闭沿恢复——6.4 判定线口径复用。
3. **真 PBR 读图**：normal/metallic/roughness 槽视觉可辨（同光源下多材质对照读图），无全白/全黑异常。
4. **材质多样性清单**：逐件材质参数+贴图槽成组记录，构成 8.2 BRDF 验收供料清单。
5. **工程闸门**：clippy 全净、库测试全过、零 VUID、exit 0。

## 待决项（开工时落定）

1. Poly Haven gltf 包形状（自含 or 贴图分离）——下载一枚实拆钉明，决定 7.3.1 补链逻辑要不要写。
2. 选件终表（按贴图槽元数据从 521 池筛）。
3. budget_image_bytes 档位（实占现算 1/2，下限 50MB）。
4. zero_day 共存与否（建议逐件进场时先 despawn 前件，独占场景读图干净——7.2 已验证整树回收安全）。

## 边界

- 不主张 BRDF/材质模型本体（参数面扩容归 8.2）；本段只证"真 PBR 贴图槽过现有链路正确"。
- 不压规模（单件小资产），规模归 7.2；不做机制扩展，缺陷走 7.4（含内嵌相机卫兵挂账）。
- Poly Haven 仅本地学习用（CC0 无分发义务）；资产不进 git（exclude 层）。

## 底座指向

- 下载通道实测记录：本计划"下载通道核实"节（2026-10-20 API 实测）
- load_world 语义与压测口径：[7.2 流程篇](../7.2-zero_day规模担当/zero_day规模压测流程：从curl到五百MB常驻走廊的各级账面.md)
- 贴图字节预算/白图保底判定线：6.4 段 README 与施工记录
