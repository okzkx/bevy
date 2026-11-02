# 施工计划：8.4 HDR、ToneMapping 与 Bloom

> 2026-10-22 立案（纯文档，随段开工立案）。对应[步骤级施工计划 §1 8.4 节](../施工计划：完整延迟渲染管线五段拆解.md)与[段 README](README.md)。本文管 8.4 段做什么、按什么顺序、验收什么。
>
> **当前状态：🔧 施工中（2026-10-22 `$plan-execute 8.4` 开工，分支 `goal/8.4`）**。

**一句话说明**：本段把合成输出从"LDR 直出（高光白截断）"补成完整 HDR 出口——HDR 域 Bloom、ACES 近似 ToneMapping、终结输出编码判定线；本段不主张 GI 与反射、不动 8.3 阴影与透明排序。

## 0. 立案期裁决（施工可推翻但须记录）

- **链路形状**：composite（HDR 线性 + EXPOSURE，8.2 形态不动）→ **Bloom 链（HDR 域）** → **present blit 扩 ToneMapping**（读 HDR + Bloom → ACES 近似 → swapchain）。present 接缝即 8.1 预留位（present_blit 模块注释钉明），不新建终点 pass。
- **Bloom 形态**：亮度阈值 prefilter（soft knee）+ 半分辨率 dual-filter 降/升采样链（Kawase 形状，5 级）→ 半分辨率 bloom 纹理；全屏三 pass（prefilter/down/up）复用 FullscreenPipeline 脚手架，资源族 `BloomTargets`（render/pipeline/post.rs）。
- **ToneMapping 曲线**：ACES 近似（Narkowicz 拟合式，常量随 shader 注释钉出处）；输出线性域交给 swapchain SRGB view 单次编码（3.4 拍板维持）——**编码恰好一次的落点 = view ROP，不动**。
- **开关与参数**：BRP `bloom_set`（enabled + strength）+ `BloomSettings` 资源（照 CullSettings 先例）；egui 面板本段不接（BRP 可用即够判定线）。
- **兼容边界**：debug 仪器模式（metallic/depth 等直出）不经 bloom/tonemap——present 层按 render mode 旁路（诊断仪器是写后读面，白截断/编码无关）。

## 1. 任务拆分

| 任务 | 交付 | 状态 |
|---|---|---|
| 8.4.1 Bloom HDR 链 | BloomTargets（半分辨率 mip 链）+ prefilter/down/up 三全屏 pass + BRP bloom_set/stats | ⬜ |
| 8.4.2 ToneMapping 接缝 | present blit 扩 ACES + bloom 叠加（开关/强度消费）；debug 模式旁路面 | ⬜ |
| 8.4.3 总验收与计量 | 亮度分布对照读图（tonemap 前后同机位）、Bloom A/B、编码单次性数学核对、回归矩阵 | ⬜ |

## 2. 判定线（照步骤级 §1 8.4 节细化）

1. **亮度分布对照**：同场景同光照同机位，tonemap 关（现行直出）vs 开——高光不再白截断、暗部不过压（读图）。
2. **Bloom 开关 A/B**：开 = 高光周围辉光可见；关 = 与纯 tonemap 读图一致。
3. **输出编码恰好一次**：数学核对（composite 线性 → ACES 线性域输出 → SRGB view ROP 唯一编码）+ 灰阶读图验证（中灰无二次 gamma 的对比度异常）。
4. **resize/最小化回归、零 VUID、WM_CLOSE exit 0、clippy 全净** + 既有判定线零回退（五脚本 + 库测试 + 8.3 shadow A/B 不受影响）。

## 3. 剩余待决（施工时钉明）

| 待决 | 说明 |
|---|---|
| Bloom 强度缺省值 | 起步 0.04 线性叠加系数，读图调 |
| 阈值 | 起步 1.0（HDR 域 >1 即光晕源）+ soft knee 0.5 |
| mip 级数 | 5 级（半分辨率逐级），读图后调 |
| debug 模式旁路落点 | present 层按 render mode 分支（mode 直出不经 ACES）——与 frame.mode UBO 通路核对 |

## 4. 测试方案

- BRP 优先：bloom_set（开关+强度）；A/B 与对照读图由 BRP 驱动。
- 读图：Sponza 中庭 + 8.3 探针球（高光截断场景）受控截图（PrintWindow+会话模型读图），取证 `.temp/8.4/`。
- 回归矩阵：hiz/lod/stream/fine/sponza 五脚本、库测试、零 VUID、WM_CLOSE exit 0、clippy 全净、8.3 shadow_stats 零 mismatch 抽测。

## 5. 边界

- 不主张：GI/反射（步骤 9）、精确 OIT（外置条目）、egui 面板接线（BRP 先行）、HDR 中间目标格式变更（R16G16B16A16_SFLOAT 现役维持——8.1 计量账不重置）。
- GPU-driven 红线不碰；8.3 shadow 链不动。
