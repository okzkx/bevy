# 8.2 Filament BRDF 与多光源光照累积（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 的着色主体段——把 Lambert 换成 Filament BRDF、把光照从"绘制时直算"改成"全屏光照累积 pass 读 G-buffer"。多光源自此与光源数解耦着色成本，本 pass 是 8.3 阴影采样与步骤 9 间接光注入的接口面。

**一句话说明**：本段把 Lambert 换成 Filament BRDF、把光照从"绘制时直算"改成"全屏光照累积 pass 读 G-buffer"——多光源自此与光源数解耦着色成本，为 8.3 阴影与步骤 9 间接光注入立接口；本段不主张阴影（归 8.3）与 GI（归步骤 9）。

**当前状态：🔧 施工中（2026-10-22 `$plan-execute 8.2` 开工，临时分支 `goal/8.2-filament-ref` 续工）**；开工前置源码参考篇已产出、段施工计划已立案（立案期拍板=光源驻留形态常驻表）；任务拆解与逐任务状态以[段施工计划 §1](施工计划：Filament%20BRDF%20与%20多光源光照累积.md)为准（下表已同步）。

## 代码落点（提案）

- **`render/shading/`**（步骤级施工计划 §5 提案形状：BRDF 着色——材质参数面、光照累积、光源数据为 8.2/8.3 主体）；逐文件去向随 8.1 迁移施工钉明。
- 落点触及既有面：材质环/实例表（8.1 迁移后路径以迁移施工记录为准）、光源组装（`scene/content/lights.rs` 扩族）、BRP 方法族扩（多光源 spawn/参数改——BRP 优先定例沿用）。

## 参考底座（2026-10-22 登记）

- **本地 Filament 源码检出 `F:\okzkx\filament`**：git 工作副本，`git describe` = `rc/1.78.1-cut-31-g373190d49`（1.78.1 前开发线，非稳定 tag，检出会漂移——参考篇产出时按当日 HEAD 重钉版本）。核实结论（2026-10-22 现场核对，属实）：
  - BRDF 数学在 `shaders/src/surface_brdf.fs`——`D_GGX` / `V_SmithGGXCorrelated` / `F_Schlick`，diffuse 走 `BRDF_DIFFUSE` 宏（Lambert 与 Burley 双变体并存，取舍随参考篇钉明）。
  - 标准模型 diffuse/specular 双 lobe 组成在 `shaders/src/surface_shading_model_standard.fs`；punctual 光距离/角度衰减在 `shaders/src/surface_light_punctual.fs`；光源循环骨架在 `shaders/src/surface_lighting.fs`。
  - PBR 理论白皮书源在 `docs_src/src_markdeep/Filament.md.html`，材质定义指南在 `docs_src/src_markdeep/Materials.md.html`。
- **开工前置 ✅ 已产出（2026-10-22，`goal/8.2-filament-ref`）**：[Filament 源码参考：标准模型 BRDF 与多光源（8.2 对照基准）](../../笔记/渲染/Filament%20源码参考：标准模型%20BRDF%20与%20多光源（8.2%20对照基准）.md)——钉明对照函数清单（D_GGX + V_SmithGGXCorrelated + F_Schlick + Lambert）、宏默认组合（desktop = FILAMENT_QUALITY_HIGH，Burley 非默认）、glTF roughness=perceptual 域换算链、punctual 衰减公式与驻留形态证据；判定线①"与 Filament 参考对照收敛"以该篇 §0/§3 钉明的函数与版本为基准。
- **边界**：Filament 是 forward 全屏管线、没有独立光照累积 pass，本段 pass 组织方式不照抄——BRDF 侧按 2026-10-22 用户拍板为**完全照抄口径**（标准模型直接光函数级移植进 WGSL，命名保留原名，详见源码参考篇 §0），pass 编排自主。

## 任务面板（随段施工计划 §1 同步）

| 任务 | 交付（详见段施工计划 §1） | 状态 |
|---|---|---|
| 8.2.1 材质参数面扩容 | `MaterialRecordLayout` 扩列（metallic/roughness 进记录、normal/AO 贴图槽进 bindless 双表）+ 解冻清单 + glTF 取数 + 7.3 供料四件接入 | ⬜ |
| 8.2.2 光源常驻表 | LightTable（storage，槽位+增量账本+free list）+ shadow 标志位 + BRP 灯方法族 + 方向光归属钉明 + 8.1 光源环切表接线 | ⬜ |
| 8.2.3 光照累积 pass | `gbuffer_composite.wgsl` Lambert 换照抄 Filament surfaceShading（函数级移植）；读 G-buffer × 常驻灯表逐灯累加；直写 HDR；间接光注入空接口 | ⬜ |
| 8.2.4 多光源判定线 | 多光源 A/B（BRP 驱动）+ 金属/粗糙度梯度读图 + 单体对照（filament 仓库资产 + 编译本体参照渲染）+ 透明层一致性 | ⬜ |
| 8.2.5 总验收与计量 | 回归总跑（resize/最小化）+ 性能成组记录（并入 8.1 七位区间）+ 既有判定线零回退 | ⬜ |

## 材料清单

- [施工计划：Filament BRDF 与多光源光照累积](施工计划：Filament%20BRDF%20与%20多光源光照累积.md)——段级任务拆解/判定线/待决/测试方案（2026-10-22 立案，含光源驻留形态拍板）。
- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——§1 8.2 节（交付/落点/判定线草案）、§2 待决项汇总（光源驻留形态）。
- [Unity PBR 调研：材质模型、BRDF 与 IBL](../../笔记/渲染/Unity%20PBR%20调研：材质模型、BRDF%20与%20IBL.md)——Filament BRDF 参照底座。
- [Filament 源码参考：标准模型 BRDF 与多光源（8.2 对照基准）](../../笔记/渲染/Filament%20源码参考：标准模型%20BRDF%20与%20多光源（8.2%20对照基准）.md)——开工前置源码参考篇（2026-10-22 产出，对照基准已钉明）。
- Filament 本地源码检出（`F:\okzkx\filament`）——BRDF 对照参考底座，锚点与开工前置见上文[参考底座](#参考底座2026-10-22-登记)。
