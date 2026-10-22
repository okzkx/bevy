# 8.2 Filament BRDF 与多光源光照累积（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 的着色主体段——把 Lambert 换成 Filament BRDF、把光照从"绘制时直算"改成"全屏光照累积 pass 读 G-buffer"。多光源自此与光源数解耦着色成本，本 pass 是 8.3 阴影采样与步骤 9 间接光注入的接口面。

**一句话说明**：本段把 Lambert 换成 Filament BRDF、把光照从"绘制时直算"改成"全屏光照累积 pass 读 G-buffer"——多光源自此与光源数解耦着色成本，为 8.3 阴影与步骤 9 间接光注入立接口；本段不主张阴影（归 8.3）与 GI（归步骤 9）。

**当前状态：🔧 施工中（2026-10-22 `$plan-execute 8.2` 开工，尾款分支 `goal/8.2.4-8.2.5` 续工）**；8.2.1~8.2.4 四任务 ✅，8.2.4 收尾（梯度读图 + DamagedHelmet 参照渲染对照）✅（2026-10-22，[施工记录](8.2.4-施工记录：多光源判定线.md)；emissive 未渲染属范围外挂账），8.2.5 未跑，**8.2.6 ✅（2026-10-22 当日立案当日收官：缺陷①②同一根因=MaterialRecord WGSL 步长 80B ≠ 宿主 64B（8.2.1 扩列引入），已修复并加 build.rs 布局互证闸门——README 前版挂账的"一批 primitive 材质全零"与"spawn 球无片元"两条随本段关闭）**——遗留项与挂账：
- **整树 despawn device lost（严重，须最小复现定责）**：Sponza 全树 BRP despawn + 103 组网格/69 张贴图同批回收后设备丢失（此前 7.2 实测同路径干净，8.1/8.2 改动面是否引入待查；重跑未复现但出现一次，另见一次疑似加载停摆/截图冻结假象）——嫌疑面：4.4 回收链 × 8.2.1 贴图槽扩列（optional_slot 三槽引用）的失效时序。
- 点光强度量级钉：BRP 缺省 candela 值对 20000 lux 方向光场景贡献 ~1e-3 输出（不可见），A/B 配方须 e3~e4 量级。
- HDR 高光白截断 = 无 ToneMapping 的预期形态（8.4 收口）；alpha mask 纹理素黑色覆盖 = 8.1 既有语义。开工前置源码参考篇已产出、段施工计划已立案（立案期拍板=光源驻留形态常驻表）；任务拆解与逐任务状态以[段施工计划 §1](施工计划：Filament%20BRDF%20与%20多光源光照累积.md)为准（下表已同步）。

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
| 8.2.1 材质参数面扩容 | `MaterialRecordLayout` 扩列（metallic/roughness 进记录、normal/AO 贴图槽进 bindless 双表）+ 解冻清单 + glTF 取数 + 7.3 供料四件接入 | ✅（2026-10-22，[施工记录](8.2.1-施工记录：材质参数面扩容.md)；冒烟实抓 VUID-06179 即修） |
| 8.2.2 光源常驻表 | LightTable（storage，槽位+增量账本+free list）+ shadow 标志位 + BRP 灯方法族 + 方向光归属钉明（set1 UBO 单光保留）+ 8.1 光源环切表接线 | ✅（2026-10-22，[施工记录](8.2.2-施工记录：光源常驻表.md)；census 55520=55520 零 mismatch，复用槽 33696=33696） |
| 8.2.3 光照累积 pass | `gbuffer_composite.wgsl` Lambert 换照抄 Filament surfaceShading（函数级移植，命名保留原名）；UBO 208B 三解冻（camera_pos）；透明层同 BRDF；间接光注入空接口 | ✅（2026-10-22，[施工记录](8.2.3-施工记录：光照累积pass照抄Filament.md)；多光源 A/B 全链读图过；TBN 两钉：uniform 控制流 + uv 退化守卫） |
| 8.2.4 多光源判定线 | 多光源 A/B（BRP 驱动）+ 金属/粗糙度梯度读图 + 单体对照（filament 仓库资产 + 编译本体参照渲染）+ 透明层一致性 | ✅（2026-10-22，[施工记录](8.2.4-施工记录：多光源判定线.md)；主体 A/B ✅ + census ✅ + mutate 热改面 ✅（edb981c91）；收尾梯度球读图 ✅ + DamagedHelmet 双渲染器对照 ✅——emissive 未渲染属范围外挂账） |
| 8.2.5 总验收与计量 | 回归总跑（resize/最小化）+ 性能成组记录（并入 8.1 七位区间）+ 既有判定线零回退 | ⬜ |
| 8.2.6 spawn 增量链缺陷修复 | 缺陷②（运行期 spawn 无片元）与缺陷①（primitive 材质记录全零）定责+修复+判定线回归；施工顺序先于 8.2.5（2026-10-22 用户指令追加，[施工计划](施工计划：spawn增量链缺陷修复（8.2.6）.md)） | ✅（2026-10-22，[施工记录](8.2.6-施工记录：spawn增量链缺陷修复.md)；**根因=MaterialRecord WGSL 步长 80B ≠ 宿主 64B**（8.2.1 扩列引入，缺陷①②同根），五个 shader 尾垫改标量 + build.rs 跨端布局互证闸门防复发；ring_census 定责仪器入 BRP 方法族） |

## 材料清单

- [8.2.4-施工记录：多光源判定线](8.2.4-施工记录：多光源判定线.md)——梯度球读图与 DamagedHelmet 双渲染器对照收尾实录（gltf_viewer settings 钉子四枚，复用必读）。
- [施工计划：Filament BRDF 与多光源光照累积](施工计划：Filament%20BRDF%20与%20多光源光照累积.md)——段级任务拆解/判定线/待决/测试方案（2026-10-22 立案，含光源驻留形态拍板）。
- [施工计划：spawn增量链缺陷修复（8.2.6）](施工计划：spawn增量链缺陷修复（8.2.6）.md)——缺陷①②定责/修复/判定线（2026-10-22 立案，8.2.5 调试仪器验收轮受控复现为定责输入）。
- [8.2.6-施工记录：spawn增量链缺陷修复](8.2.6-施工记录：spawn增量链缺陷修复.md)——定责方法链实录（环读回→gap 哨兵逐条件排除）、根因与修复、代码改动解析。
- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——§1 8.2 节（交付/落点/判定线草案）、§2 待决项汇总（光源驻留形态）。
- [Unity PBR 调研：材质模型、BRDF 与 IBL](../../笔记/渲染/Unity%20PBR%20调研：材质模型、BRDF%20与%20IBL.md)——Filament BRDF 参照底座。
- [Filament 源码参考：标准模型 BRDF 与多光源（8.2 对照基准）](../../笔记/渲染/Filament%20源码参考：标准模型%20BRDF%20与%20多光源（8.2%20对照基准）.md)——开工前置源码参考篇（2026-10-22 产出，对照基准已钉明）。
- Filament 本地源码检出（`F:\okzkx\filament`）——BRDF 对照参考底座，锚点与开工前置见上文[参考底座](#参考底座2026-10-22-登记)。
