# 8.6 还原 Filament 完整 ACES（M7 追加段）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：治"受光面进曲线肩部后饱和度坍塌"的发灰观感（Sponza unlit ~23% vs shaded ~1% 实测）——还原 Filament 完整版 ACES 色调映射 + 曝光锚点重校，为步骤 9 IBL 的最终成像链铺路。

**一句话说明**：本段把 present 链的裸 Narkowicz 逐通道拟合替换为 Filament 完整版 `aces::ACES`（AP0/AP1 色域 + glow + 红修正 + Hill 拟合 + 暗包围，十步逐式照抄），亮度增益位与曝光锚点分离外露；不主张 PBR Neutral、grading 全链与 EVILS（边界见段施工计划），暗部无色相归步骤 9 间接光。

**当前状态：✅ 收官（2026-10-23 当日立案当日施工，分支 `goal/8.6`，常驻 worktree `F:\okzkx\bevy-wt`）**——判定线五条全过（饱和度差收窄/彩旗读图/编码恰好一次/零 VUID+exit 0+clippy+库测试 41/41/回归五脚本零回退），证据见[施工记录](8.6-施工记录：还原Filament完整ACES与曝光重校.md)；EXPOSURE 重校 0.00016、默认组合=filament+gain1.667（Filament viewer ACES_LEGACY 口径，A/B 订正）。流程走读见[功能流程导读](8.6-功能流程导读：色调映射三态曲线通道.md)。

## 代码落点

- `shaders/present_tonemap.wgsl`（十步管线照抄 + curve/gain 参数）；`shaders/gbuffer_composite.wgsl`（EXPOSURE 锚点重校；8.6.5 补介电体 F0 换算）；`shaders/transparent_forward.wgsl`（8.6.5 同步 F0 换算与曝光锚点）。
- `render/pipeline/post.rs`（BloomParams/BloomSettings 扩容）；`remote/bloom.rs`（`ash_renderer/bloom_set` 扩 curve/gain）；`overlay/windows/pipeline_window.rs`（三态对照 UI）。
- 前置件：`remote/mod.rs`（BRP 端口可配 `ASH_BRP_PORT`）。

## 任务面板

| 任务 | 内容 | 状态 |
|---|---|---|
| 8.6.1 前置件 | BRP 端口可配（双实例并行取证） | ✅ |
| 8.6.2 曲线替换与参数面 | present_tonemap 十步管线照抄 + curve/gain 参数面四件（UBO/资源/BRP/窗口） | ✅ |
| 8.6.3 曝光锚点重校 | Sponza 三态 A/B：EXPOSURE 重标 0.00016 + 饱和度差实测 | ✅ |
| 8.6.4 判定线总验收 | 判定线五条 + 回归五脚本 + 性能成组 | ✅ |
| 8.6.5 收官后缺陷修复 | 介电体 F0 漏 `computeDielectricF0`（F0=0.5 致受光面叠白高光发灰）+ transparent 曝光同锚；Filament 同场景对照饱和度对齐 | ✅ |

## 材料清单

- [施工计划：还原Filament完整ACES与曝光锚点重校](施工计划：还原Filament完整ACES与曝光锚点重校.md)——范围边界、开工拍板、任务拆解与判定线。
- [施工记录：还原Filament完整ACES与曝光重校](8.6-施工记录：还原Filament完整ACES与曝光重校.md)——逐任务实录、四态 A/B 实测数据与判定线证据。
- [功能流程导读：色调映射三态曲线通道](8.6-功能流程导读：色调映射三态曲线通道.md)——从 BRP 请求到 swapchain 写出的数据流走读。
- [Filament 源码参考：色调映射与 Color Grading（8.6 对照基准）](../笔记/渲染/Filament%20源码参考：色调映射与%20Color%20Grading（8.6%20对照基准）.md)——开工前置源码参考篇（十步管线/Hill 常量/EVILS 边界/发灰归因基线）。
