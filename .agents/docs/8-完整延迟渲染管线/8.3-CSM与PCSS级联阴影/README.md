# 8.3 CSM + PCSS 级联软阴影（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：给多光源里的方向光接 CSM 级联 shadow map + PCSS 软阴影；shadow depth pass 走 GPU-driven（逐 cascade 复用标记链与 build_indirect，CPU 只发矩阵），阴影采样进 8.2 光照累积 pass。

**一句话说明**：本段给多光源里的方向光接 CSM 级联 shadow map + PCSS 软阴影，shadow depth pass 走 GPU-driven（逐 cascade 复用标记链与 build_indirect，CPU 只发 cascade 矩阵）；本段不主张 VSM（外置条目 7）、不做阴影降噪时域滤波。

**当前状态：⬜ 已立案（2026-10-21）**；段目录与段 README 预立（2026-10-22 用户增补，预施工文件夹先行）——段施工计划与任务编号仍随段开工立案，判定线以[步骤级施工计划 §1 8.3 节](../施工计划：完整延迟渲染管线五段拆解.md)为准。

## 代码落点（提案）

- **`render/shading/`** 为主（阴影采样进 8.2 光照累积）；shadow depth pass 编排归 `render/pipeline/`（步骤级施工计划 §5 提案形状，随段施工计划钉明）。

## 任务面板（拆分占位）

任务编号与逐任务交付随段施工计划立案钉明；下表为步骤级交付范围的预分，不预设编号。

| 任务（占位） | 内容 |
|---|---|
| shadow depth pass 逐 cascade GPU-driven | cascade 视锥代主相机视锥进 5.2 六平面标记（新标记族或分列施工时钉明），标记后 build_indirect 生成 shadow draw——CPU 只发 cascade 矩阵，剔除/决策归 GPU 既有链（行业同构=GPU-driven shadow culling） |
| 两枚语义钉执行 | ①遮挡标记不参与 shadow pass——Hi-Z 金字塔是主相机深度语义，对 shadow 视锥不合法，shadow pass 不做遮挡剔除是行业默认；②shadow draw 取主 LOD 档**再粗一档**——6.2 档位是主相机覆盖语义，shadow 视角直接沿用会过细（2026-10-20 复核钉入，照此执行） |
| 级联划分与 PCSS | 级联数量/分割比例施工时钉明；PCSS 滤波半径（blocker search + 半影估计） |
| 阴影采样进光照累积 | 8.2 光源记录的 shadow 标志位由本段接管采样选择权 |
| 判定线与计量 | 阴影开关 A/B；级联边界无跳变（读图）；逐 cascade `draw_calls ≤ 该级可见数`（cull_stats 对照口径复用，5.2 同款）；性能成组记录（shadow pass 计时区间并入 5.5 计量底座） |

## 材料清单

- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——§1 8.3 节（交付/判定线草案，含两枚语义钉）。
- [外置拓展需求条目 7：VSM](../../外置拓展需求.md)——阴影技术边界（不进本段，收益绑定 Nanite 级几何负载与页管理工程量）。
