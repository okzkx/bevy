# 施工计划：8.2 Filament BRDF 与多光源光照累积

> 2026-10-22 立案（用户拍板光源驻留形态=常驻表后立案，纯文档）。对应[步骤级施工计划 §1 8.2 节](../施工计划：完整延迟渲染管线五段拆解.md)与[段 README](README.md)：把 Lambert 换成 Filament BRDF、把光照从"绘制时直算"改成"全屏光照累积 pass 读 G-buffer"。本文管 8.2 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：⬜ 已立案（2026-10-22），未开工**。

**一句话说明**：本段把材质参数面扩成 PBR 四件（metallic/roughness/normal/AO）、把光源立成常驻表、把 8.1 合成 pass 的单方向光 Lambert 换成读 G-buffer × 逐灯累加的 Filament BRDF 光照累积——多光源自此与光源数解耦着色成本，为 8.3 阴影采样（shadow 标志位）与步骤 9 间接光注入（间接项只立接口）立接口；本段不主张阴影本体、IBL 与 GI，代码落 `render/shading/` 与既有 `render/pipeline/`/`render/assets/` 扩展。

## 0. 立案期裁决（2026-10-22 用户拍板）

- **光源驻留形态=常驻表**：灯数据走 set2 式常驻 storage 表（对象表/材质表同族），增量账本驱动更新（4.1 对象增量先例），不逐帧全量上传。光源记录**预留 shadow 标志位**（8.3 接管采样选择权）；Filament 的 froxel 灯列表与本段无冲突——8.1 已建的 3D cluster 逐 tile 光源列表（存灯索引）改从常驻表取数，索引域统一到表槽号。参考证据：[Filament 源码参考 §5.3](../../笔记/渲染/Filament%20源码参考：标准模型%20BRDF%20与%20多光源（8.2%20对照基准）.md)（Filament 本体走每帧 UBO+froxel，无常驻表；常驻表是本项目按 4.1/5.1 账本路线的自选形态，收益=稳态零上传）。
- **BRDF 对照基准已钉明**（开工前置参考篇已产出 2026-10-22，`goal/8.2-filament-ref`）：D_GGX + V_SmithGGXCorrelated（Heitz 完整）+ F_Schlick（f90=saturate(16.5·Σf0)）+ diffuse=Lambert；desktop=QUALITY_HIGH 档，Burley 不取；glTF roughnessFactor=perceptualRoughness（clamp 0.045→α=p²）；energyCompensation=1.0（无 IBL）。
- **对照资产与参照渲染解锁（2026-10-22 用户增补拍板）**：①对照资产可直接用 filament 仓库自带（属实 2026-10-22 现场清点：`third_party/models/` 八件 glTF 含 DamagedHelmet.glb 3.8MB、`assets/models/` 十二件含 material_sphere、`assets/environments/white_furnace` 白炉环境）——Khronos MetalRoughSpheres 若仍需要走 Sponza 同源重下通道；②参照渲染可编译 filament 本体（`build.sh` windows 分支 + `gltf_viewer` 载入同一 glb 同参数截图对照）——**编译是否顺待实测**（本仓库 checkout 尚未编译过，`build/` 只有构建模板；顺带得到判定线①从"读图对照"升级为"与 filament 实渲染对照"的口径，8.2.4 施工时钉实）。

## 1. 任务拆分

| 任务 | 交付 | 状态 |
|---|---|---|
| 8.2.1 | 材质参数面扩容：`MaterialRecordLayout` 扩列（metallic/roughness 标量进记录、normal/AO 贴图槽进 bindless 双表）；扩列解冻清单（照 5.1.1 先例落清单）；glTF 侧 metallic/roughness/normal/AO 取数与域语义（roughness=perceptual 域）；7.3 供料四件（真 PBR 贴图面）接入验证 | ⬜ |
| 8.2.2 | 光源常驻表：LightTable（storage，槽位+增量账本+free list，4.1 先例）；记录含 pos/radius、color×intensity、type、**shadow 标志位**；BRP 灯方法族扩（spawn/参数改/stats，`tools/brp.py` 子命令）；方向光归属钉明（set1 UBO 单光保留 vs 并表）；8.1 光源环（`TestPointLights` 每帧环）与 cluster 逐 tile 列表切表接线 | ⬜ |
| 8.2.3 | 光照累积 pass：`gbuffer_composite.wgsl` 的 Lambert 换 surfaceShading 同构（D_GGX+V_Smith+F_Schlick+Lambert，NoL 折尾部、occlusion 乘光源贡献）；读 G-buffer（albedo/normal/深度重建世界位置）× 常驻灯表逐灯累加；输出直写 HDR 中间目标（8.1 HDR blending 前置闸门已实测通过）；间接光注入项立空接口不填内容（步骤 9） | ⬜ |
| 8.2.4 | 多光源判定线：多光源开关 A/B（BRP 灯族驱动）；金属/粗糙度梯度读图（MetalRoughSpheres 式梯度球）；Khronos MetalRoughSpheres/DamagedHelmet 单体对照（3.5 受控对照法，漫反射项收敛口径见参考篇 §0/§3）——资产用 filament 仓库自带 + **编译 filament 本体做参照渲染**（gltf_viewer 同 glb 同参数对照，编译待实测）；透明层与不透明累积同光源同 BRDF 一致性 | ⬜ |
| 8.2.5 | 判定线总验收与计量：回归总跑（resize/最小化）、性能成组记录（光照累积计时并入 8.1 七位区间，5.5 计量底座复用）、既有判定线零回退 | ⬜ |

## 2. 判定线草案（段级）

1. **漫反射项与 Filament 参考对照收敛**（参考=本地检出 `F:\okzkx\filament` @ `373190d49`，对照基准=参考篇 §0/§3；Khronos MetalRoughSpheres/DamagedHelmet 单体对照，沿用 3.5 受控对照法；参照渲染优先编译 filament 本体——资产用其仓库自带，编译待实测翻车回退读图对照）。
2. **多光源开关 A/B**：同一场景单灯/多灯读图，多灯贡献只来自常驻灯表逐灯累加；census 对照（灯表内容 vs BRP 读数）零 mismatch。
3. **金属/粗糙度梯度读图**：金属度 0→1、粗糙度低→高梯度球面，高光形状与 F0 语义正确（金属高光带 albedo 色、介电体白高光）。
4. **既有判定线零回退**：4.x 零泄漏、5.3 draw_calls 口径（GPU-driven 红线）、8.1 四模式读图与 cluster census、零 VUID、exit 0、clippy 全净。

## 3. 剩余待决（施工时钉明）

| 待决 | 说明 |
|---|---|
| 方向光归属 | set1 UBO 单方向光保留（灯表只装 punctual）vs 并入常驻表——倾向前者（方向光是全局量语义、bevy GPU 同款现役链不动），8.2.2 钉明 |
| normal 贴图 TBN 来源 | 顶点 tangent 可用性核实（glTF 资产/tangent 槽现役面）；无 tangent 资产的派生方案（屏幕空间导数）取舍 |
| AO 语义 | 直接光乘法（Filament occlusion 尾乘，参考篇 §4）为 8.2 交付；环境 AO（multiBounceAO）归间接光（步骤 9），本段不取 |
| 材质环扩列解冻清单 | 扩哪些 set/binding 声明、布局断言同步面（5.1.1 先例落 8.2.1） |
| 对照资产获取通道 | ✅ 已拍板（2026-10-22 用户增补）：filament 仓库自带资产优先（DamagedHelmet.glb 等，§0 清点属实）；MetalRoughSpheres 若仍需要走 Sponza 同源重下通道 |
| filament 参照渲染 | 编译 filament 本体（build.sh windows 分支 + gltf_viewer）做同 glb 同参数对照——编译待实测，8.2.4 施工时钉实；翻车回退纯读图对照口径 |
| LDR/HDR 中转 | 输出直写 HDR 中间目标为计划口径（8.1 blending 已实测）；如实测翻车回退 LDR 中转，8.2.3 钉明 |

## 4. 测试方案

- **BRP 优先**：灯方法族（spawn/参数改/remove/stats）+ 材质参数热改（mutate 先例），`tools/brp.py` 子命令化；判定线 2/3 的 A/B 与梯度由 BRP 驱动，不写一次性探针。
- **受控对照**：MetalRoughSpheres/DamagedHelmet 单体场景截图读图（PrintWindow+会话模型读图，画面功能视觉验证规范）；**参照渲染优先编译 filament 本体**（gltf_viewer 载入同一 glb、同光源参数，双截图对照——资产与白炉环境用其仓库自带），编译不顺则回退读图对照；漫反射项收敛以"与参考实现数值对照"为口径（同参数下 Lambert 项逐位一致可算，specular 项画面对照）。
- **回归矩阵**：resize/最小化、cluster census、hiz/lod/stream/fine 四脚本、库测试、零 VUID、WM_CLOSE exit 0、clippy 全净。

## 5. 边界

- **不主张阴影**：shadow depth pass、级联与 PCSS 全归 8.3；本段只预留 shadow 标志位与采样接口位。
- **不主张 IBL 与 GI**：环境光项维持现役 ambient 常量（灯表外），间接光注入项只立接口；DFG LUT/多重散射补偿归步骤 9 附近。
- **不主张 HDR 合成契约变化**（归 8.4）与精确 OIT（外置条目）。
- **GPU-driven 闭环不回退是红线**：光照累积是新增全屏 pass，不动 5.3 draw 决策链。
- 依赖顺序：8.1 收官（已收官待并回）→ 8.2 → 8.3；开工前置参考篇已产出（[源码参考篇](../../笔记/渲染/Filament%20源码参考：标准模型%20BRDF%20与%20多光源（8.2%20对照基准）.md)）。
