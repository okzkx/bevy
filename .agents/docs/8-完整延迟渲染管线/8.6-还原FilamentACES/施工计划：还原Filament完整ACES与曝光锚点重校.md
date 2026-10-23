# 施工计划：还原 Filament 完整 ACES 与曝光锚点重校（8.6）

**段**：8.6（步骤 8 追加段，2026-10-23 用户增补立案；本计划随段开工立案）
**分支**：`goal/8.6`（起点 cfb38bd66；常驻 worktree `F:\okzkx\bevy-wt` 施工）
**施工依据**：[源码参考篇：色调映射与 Color Grading（8.6 对照基准）](../笔记/渲染/Filament%20源码参考：色调映射与%20Color%20Grading（8.6%20对照基准）.md)（开工前必读；本地检出 `F:\okzkx\filament` @ 373190d49）
**照抄口径**：8.2 参考篇同款——函数级移植、命名保留原名

## 一、范围与边界

**主张**：present 链的裸 Narkowicz 逐通道拟合替换为 Filament 完整版 `aces::ACES`（AP0/AP1 色域 + glow 模块 + 红色修正 + Hill RRT+ODT 拟合 + 暗包围补偿，十步逐式照抄）；亮度增益位（ACES vs ACES_LEGACY 的 1.667）与曝光锚点分离外露；曝光锚点重校（典型受光面落线性段 ~0.5–0.8）；开关挂现有 `bloom_set`/管线控制窗口做三态 A/B（裸 Narkowicz 保留作对照基线）。

**不做**（立案拍板，恢复条件见参考篇 §六）：PBR Neutral；grading 全链（白平衡/CDL/vibrance/saturation/LUT 烘焙）；EVILS 亮度缩放与 gamutMapping；LUT 化（全屏 pass 直算成本可接受）；暗部无色相归步骤 9 间接光。

**开工拍板一（色域口径）**：Filament 的 `ToneMapper` 契约是 linear Rec.2020 进出（`ToneMapper.h:67-73`；grading 链经 `combinedInTransform`=sRGB_to_Rec2020 后才进 tonemapper，`ColorGrading.cpp:710/1073`）。本渲染器 HDR 链全程 linear sRGB，故 WGSL 内进曲线前 `sRGB_to_Rec2020`、出曲线后 `Rec2020_to_sRGB`；基底矩阵照 `ColorSpaceUtils.h` 逐值照抄，合成式同源（`sRGB_to_Rec2020 = XYZ_to_Rec2020 * sRGB_to_XYZ`）。输出编码仍归 swapchain SRGB view ROP 恰好一次（3.4 拍板不破）。

**开工拍板二（增益位形状）**：`BloomParams`/`BloomSettings` 扩 `curve`（0=Narkowicz 基线 / 1=Filament ACES）与 `gain`（f32，默认 1.0）。LEGACY 形态=gain 1.667，不单设枚举位——增益与曝光锚点分离外露正是本段主张；三态 A/B=Narkowicz / ACES(gain 1.0) / ACES(gain 1.667)。曝光锚点仍挂 `gbuffer_composite.wgsl` 的 `EXPOSURE`，重校随 8.6.3 A/B 定值。

## 二、任务拆解

| 任务 | 内容 | 状态 |
|---|---|---|
| 8.6.1 前置件 | BRP 端口可配：`ASH_BRP_PORT` 环境变量（默认 15702 不变，render subapp 跟随 +1），双实例并行取证各占一段端口 | ✅（2a164678d） |
| 8.6.2 曲线替换与参数面 | `present_tonemap.wgsl` 十步管线照抄（矩阵/glow/红修正/Hill 常量/暗包围，执行序见参考篇 §二）；BloomParams/BloomSettings/`bloom_set`/管线控制窗口扩 curve+gain | ⬜ |
| 8.6.3 曝光锚点重校 | Sponza 同机位三态 A/B：EXPOSURE 重标（受光面落线性段 ~0.5–0.8 而非肩部 1.7）、unlit vs shading 饱和度差实测；量纲改动如实留痕（8.2 DamagedHelmet 曝光标定口径受牵动，重标） | ⬜ |
| 8.6.4 判定线总验收 | §三判定线逐条 + 回归五脚本 + 性能成组 | ⬜ |

## 三、判定线（验收）

1. 同机位同光照 unlit vs shading 饱和度差实测收窄（现象基线：受光面 ~1%，参考篇 §八；收窄阈值随 8.6.3 实测钉明）
2. 高饱和内容（Sponza 彩旗）高光段读图无明显褪色变相（三态对照）
3. 输出编码恰好一次不破；tonemap 关（A/B 的 B 面）与 debug 仪器旁路面行为不变
4. 零 VUID / WM_CLOSE exit 0 / clippy 全净 / 库测试全绿
5. 回归五脚本零回退（hiz/lod/stream/fine/sponza）；draw_calls 维持 5.3 口径红线

## 四、测试方案

- 手段三级递进：BRP 首选（`bloom_set` 扩 curve/gain、`set_render_mode` unlit/shaded 切换）→ PrintWindow 截图读图（《画面功能视觉验证》，取证进根目录 `.temp/`）；不涉鼠标注入。
- 性能成组：present tonemap pass 计时（十步管线 vs Narkowicz 对照，现况底噪以 `perf_stats` 读数为基）。
- 回归：hiz_test/scene_lod/scene_stream/scene_fine/scene_sponza 五脚本照单全跑。
