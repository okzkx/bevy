# 8.5 SMAA 抗锯齿（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 收口段——tonemap 后接 SMAA 三 pass，作为延迟管线（不用 MSAA）的抗锯齿标准搭档，完成从 G-buffer 到 swapchain 直出的完整链路。

**一句话说明**：本段 tonemap 后接 SMAA 三 pass（边缘检测/权重计算/邻域混合），作为延迟管线的抗锯齿标准搭档收口步骤 8；本段不主张 TAA 与任何时域手段。

**当前状态：✅ 收官（2026-10-23）**——判定线四条全过、回归五脚本零回退；分支 `goal/8.5`（2026-10-22 代码完成时运行级验证未过如实留存，2026-10-23 续工修复三枚缺陷后收官）。判定线证据与缺陷实录见[施工记录](8.5-施工记录：SMAA三pass.md)；技术决策补档见[段施工计划](施工计划：SMAA三pass.md)。

## 代码落点

- `render/pipeline/smaa.rs`（SmaaTargets/SmaaPipelines）；`shaders/smaa_edge.wgsl`/`smaa_blend.wgsl`/`smaa_neighborhood.wgsl`（官方 1x 拆分）；接线 `render/pipeline/frames.rs`（tonemap 落点 LDR → edge/blend → neighborhood）+ `driver/host/frame.rs`（闸门③重建清单）。
- BRP：`ash_renderer/smaa_set`（tools/brp.py `smaa-set true|false`）。

## 任务面板

| 任务 | 内容 | 状态 |
|---|---|---|
| SMAA 三 pass | 边缘检测/权重计算/邻域混合（LDR 域、tonemap 后）；LUT 来源=官方预生成 texture（bevy_anti_alias ktx2 → PNG 嵌入直传） | ✅ |
| 判定线与计量 | 几何边缘锯齿读图 A/B（diff 热力图 + 4× 放大）、静止/相机移动连拍、性能成组（低于测量底噪）、零 VUID/exit 0/clippy 全净/回归五脚本 | ✅ |

## 材料清单

- [施工计划：SMAA三pass（立案补档）](施工计划：SMAA三pass.md)——技术决策与任务拆解（as-built）。
- [施工记录：SMAA三pass](8.5-施工记录：SMAA三pass.md)——判定线证据、续工三枚缺陷实录与回归矩阵。
- [步骤级施工计划 §1 8.5 节](../施工计划：完整延迟渲染管线五段拆解.md)——判定线草案出处。
