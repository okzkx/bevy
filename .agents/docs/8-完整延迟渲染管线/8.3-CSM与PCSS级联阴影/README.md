# 8.3 CSM + PCSS 级联软阴影（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：给多光源里的方向光接 CSM 级联 shadow map + PCSS 软阴影；shadow depth pass 走 GPU-driven（逐 cascade 复用标记链与 build_indirect，CPU 只发矩阵），阴影采样进 8.2 光照累积 pass。

**一句话说明**：本段给多光源里的方向光接 CSM 级联 shadow map + PCSS 软阴影，shadow depth pass 走 GPU-driven（逐 cascade 复用标记链与 build_indirect，CPU 只发 cascade 矩阵）；本段不主张 VSM（外置条目 7）、不做阴影降噪时域滤波。

**当前状态：✅ 收官（2026-10-22，五任务全过，[总验收账](8.3.5-施工记录：判定线总验收与计量.md)；分支 `goal/8.3` 待并回拍板）**；五判定线全过（A/B / 级联边界无跳变 / 逐 cascade 对照零 mismatch / 性能成组 / 零 VUID+clippy+库测试 41/41），回归矩阵与 8.2.5 基线一致（lod 6/7 = 挂账 F25 同形态）。；段施工计划已立案（[施工计划：CSM级联阴影与PCSS软阴影](施工计划：CSM级联阴影与PCSS软阴影.md)——立案期裁决钉明：4 级 log 分割 0.5→80m、D32 2048²×4、cull 单 dispatch 内联四 cascade、shadow_build 独立 pass、4 小 render pass、PCSS texture_load 路线、shadow UBO 新族、两枚语义钉照抄）；任务编号与逐任务状态以[段施工计划 §1](施工计划：CSM级联阴影与PCSS软阴影.md)为准（下表已同步）。

## 代码落点（提案）

- **`render/shading/`** 为主（阴影采样进 8.2 光照累积）；shadow depth pass 编排归 `render/pipeline/`（步骤级施工计划 §5 提案形状，随段施工计划钉明）。

## 任务面板（拆分占位）

五任务全过（2026-10-22），逐任务交付与状态：

| 任务 | 交付（详见段施工计划 §1） | 状态 |
|---|---|---|
| 8.3.1 级联底座 | 宿主级联矩阵（log 分割 0.5→80m + 光空间 AABB 拟合）+ shadow UBO 帧槽环（704B）+ 4×D32 2048² 附件族 + shadow_write 深度只写管线 + ShadowSettings/BRP shadow_set | ✅（[施工记录](8.3.1-施工记录：CSM级联底座.md)） |
| 8.3.2 shadow 标记与命令 | cull 内联四 cascade 标记（[槽][cascade] 交错环）+ shadow_build 四命令环（两枚语义钉执行面）+ shadow_stats 对照读数 | ✅（[施工记录](8.3.2-施工记录：shadow标记、命令与depthpass.md)） |
| 8.3.3 shadow depth pass | 四 render pass 逐 cascade indirect draw + gpu_shadow_ms 计时段（T2 插入段界顺移，时间戳 7→8 位） | ✅（与 8.3.2 合并施工，同上） |
| 8.3.4 PCSS 采样进光照累积 | composite @group(3)（texture_load 路线）+ blocker search/半影 PCF + 法线偏移与级联序号容差；方向光项乘可见性 | ✅（[施工记录](8.3.4-施工记录：PCSS采样进光照累积.md)；bias 符号反/重建噪声放大两钉实录） |
| 8.3.5 总验收与计量 | 判定线五条 + 回归矩阵（五脚本 + 库测试 41/41 + 零 VUID） | ✅（[总验收账](8.3.5-施工记录：判定线总验收与计量.md)） |

## 材料清单

- [8.3-功能流程导读：从方向光到PCSS阴影的各级账面](8.3-功能流程导读：从方向光到PCSS阴影的各级账面.md)——一帧内逐时刻走读 + 新文件分工 + 改动解析（2026-10-22）。
- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——§1 8.3 节（交付/判定线草案，含两枚语义钉）。
- [外置拓展需求条目 7：VSM](../../外置拓展需求.md)——阴影技术边界（不进本段，收益绑定 Nanite 级几何负载与页管理工程量）。
