# 8.4 HDR、ToneMapping 与 Bloom（M7）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：把不透明+透明的合成从 LDR 直出改成 HDR 契约——高动态中间目标、Bloom、tone mapping，并终结 3.4 起挂账的 swapchain 输出编码判定线悬案，为 8.5 SMAA 提供 tonemap 后的输入。

**一句话说明**：本段把合成从 LDR 直出改成 HDR 契约——高动态中间目标、Bloom、tone mapping、输出编码恰好一次，终结 swapchain 编码判定线悬案；本段不主张 GI（归步骤 9）与反射。

**当前状态：✅ 收官（2026-10-22，[施工记录](8.4-施工记录：HDR、ToneMapping与Bloom.md)，分支 `goal/8.4` 待并回拍板）**；段施工计划已立案（[施工计划：HDR、ToneMapping与Bloom](施工计划：HDR、ToneMapping与Bloom.md)）——链路=present 接缝正式启用（blit 退役），Bloom 5 级 dual-Kawase + ACES（Narkowicz 近似）+ 编码恰好一次落点维持 SRGB view ROP；四判定线全过（亮度分布对照/Bloom A/B/编码单次性/resize+零 VUID+clippy），回归矩阵与 8.3 基线一致。

## 代码落点（提案）

- **`render/post/`**（步骤级施工计划 §5 提案形状：后处理——HDR/Bloom/ToneMapping/SMAA 为 8.4/8.5 主体）；逐文件去向随 8.1 迁移施工钉明。

## 任务面板（拆分占位）

任务编号与逐任务交付随段施工计划立案钉明；下表为步骤级交付范围的预分，不预设编号。

| 任务（占位） | 内容 |
|---|---|
| 合成进 HDR 契约 | 中间渲染目标改浮点格式（8.1 已留位，本段正式启用）；swapchain 写入只发生在 tone mapping 之后 |
| Bloom | HDR 域，亮度阈值 + 降采样模糊链——实现形态施工时钉明 |
| ToneMapping | 曲线选型施工时钉明（ACES 近似族为默认候选） |
| 输出编码恰好一次 | UNORM 底板 + SRGB view（3.4 拍板）与 HDR/tone mapping 链的衔接落点施工时钉死；编码次数数学核对入判定线 |
| 判定线与计量 | 与现行 LDR 直出亮度分布对照（同场景同光照读图）；Bloom 开关 A/B；输出编码单次性验证（灰阶/亮部不重复 gamma）；resize/最小化回归、零 VUID、exit 0、clippy 全净 |

## 材料清单

- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——§1 8.4 节（交付/判定线草案）、§2 待决项汇总（输出编码落点）。
