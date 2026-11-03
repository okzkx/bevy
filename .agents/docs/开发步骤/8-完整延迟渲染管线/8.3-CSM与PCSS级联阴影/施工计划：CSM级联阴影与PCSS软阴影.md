# 施工计划：8.3 CSM 级联阴影与 PCSS 软阴影

> 2026-10-22 立案（纯文档，随段开工立案钉明任务编号）。对应[步骤级施工计划 §1 8.3 节](../施工计划：完整延迟渲染管线五段拆解.md)与[段 README](README.md)：给方向光接 CSM 级联 shadow map + PCSS，shadow depth pass 走 GPU-driven。本文管 8.3 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：⬜ 已立案（2026-10-22）**。

**一句话说明**：本段给多光源里的方向光接 CSM 级联 shadow map + PCSS 软阴影——shadow depth pass 逐 cascade 复用 5.2 标记链与 5.3 命令组装（cascade 视锥代主相机视锥，CPU 只发矩阵），PCSS 采样进 8.2 光照累积 pass；本段不主张 VSM（[外置条目 7](../../外置拓展需求.md)）、不做时域滤波、不主张点光源阴影（LightRecord shadow bit0 保持记账，punctual shadow 归后继段）。

## 0. 立案期裁决（本计划钉明，施工可推翻但须记录）

- **两枚语义钉照抄（2026-10-20 复核钉入）**：①Hi-Z 遮挡标记不参与 shadow pass（主相机深度语义对 shadow 视锥不合法，shadow 不做遮挡剔除=行业默认）；②shadow draw 取主 LOD 档**再粗一档**（6.2 档位是主相机覆盖语义，直接沿用会过细）。
- **级联数量=4，log 分割**：覆盖 near 0.5m → far 80m（Sponza 世界尺度 ~30×12×18m + 梯度球阵 y=25 的对照余量）；分割比例施工时读图微调。shadow map = 4× D32_SFLOAT 2048²（reverse-Z，clear=1.0，与主深度同方向）。
- **shadow 剔除=单 dispatch 内联四 cascade**：`cull.wgsl` 扩展——invocation 对每 cascade 各测一次 AABB vs cascade 视锥（平面由宿主 Gribb-Hartmann 算好进 shadow UBO，4×6 平面 384B 超 push 上限故走 UBO），写四份 shadow 标记（新 storage 族，读 shadow UBO）。不四重派发（省三遍对象表扫描）。
- **shadow 命令组装=独立 pass**：`shadow_build.wgsl`（compute 单入口）——读在场环 + shadow 标记 + LOD 档位环（再粗一档 clamp 到常驻哨兵），写四条 shadow 命令环（20B IndirectCommand/条/环）。不并进 build_indirect（该 pass 已 9 binding，职责分家照 cull/occlusion 先例）。
- **shadow depth pass=4 个独立小 render pass**（每 cascade 一个：clear→一次 `cmd_draw_indexed_indirect`→end，同一 command buffer）；light view_proj 走 push（64B）；新 `shadow_write.wgsl`（顶点变换直出，深度只写、无色附件；独立 graphics pipeline，depthAttachmentFormat=D32 声明面照 VUID-08914 钉）。
- **shadow 采样=PCSS**（composite 直连）：D32 深度图 `texture_load` 整数坐标自写滤波（non-filterable 路线，不引 compare sampler）；cascade 选择按视深（世界位置到相机距离 vs 分割带）；blocker search + 半影估计 + PCF 采样核；光照累积的方向光项乘 shadow 可见性。
- **shadow UBO 独立新族**（帧槽环，wait_for_slot fence 纪律）：cascade_view_proj[4] + cascade 分割参数 + 平面族（384B）≈ 640B/set；不动已满的帧 UBO 192B。
- **开关**：`ShadowSettings` 资源（照 CullSettings 先例）+ BRP `ash_renderer/shadow_set`（enabled）；关=shadow 剔除/命令/绘制/采样全链旁路（与 hiz/lod/occlusion 的 A/B 同构），8.2 基线画面零变化。
- **8.2 LightRecord shadow bit0**：方向光不在灯表（set1 UBO 单光语义，8.2.2 拍板保留），本段 shadow 开关作用于方向光；灯表 bit0 语义不变，punctual shadow 归后继段。

## 1. 任务拆分

| 任务 | 交付 | 状态 |
|---|---|---|
| 8.3.1 级联底座 | 宿主侧级联矩阵计算（光空间视锥拟合 + log 分割 + 世界 texel snap）+ shadow UBO 帧槽环 + 4×D32 2048² 深度附件族 + `ShadowSettings`/BRP shadow_set + `shadow_write.wgsl` 与深度只写管线（空转首跑=不接画面） | ⬜ |
| 8.3.2 shadow 标记与命令 | cull.wgsl 内联四 cascade 标记（A/B 关=全可见填充）+ `shadow_build.wgsl` 四命令环（LOD 再粗一档）+ shadow draw_calls 对照读数（BRP shadow_stats） | ⬜ |
| 8.3.3 shadow depth pass | 四 render pass 录制（逐 cascade indirect draw）+ `gpu_shadow_ms` 计时区间进 5.5 底座 | ⬜ |
| 8.3.4 PCSS 采样进光照累积 | composite 接 shadow 图采样（cascade 选择 + blocker search + 半影 PCF）+ shadow 开关全链消费 + 影子读图 | ⬜ |
| 8.3.5 总验收与计量 | 判定线总跑（A/B/边界读图/draw_calls 对照/性能成组）+ 回归矩阵（hiz/lod/stream/fine/sponza 五脚本 + 库测试 + 零 VUID + WM_CLOSE exit 0 + clippy） | ⬜ |

## 2. 判定线（段级，照步骤级 §1 8.3 节细化）

1. **阴影开关 A/B**：BRP shadow_set 驱动；开=影子可见（地面/墙面接触影读图），关=与 8.2 基线读图结构一致（同机位对照）。
2. **级联边界无跳变**：沿深度方向的分割带读图，阴影质量渐变、无硬缝/无影长突变（近/中/远三机位）。
3. **逐 cascade `draw_calls ≤ 该级可见数`**：shadow 标记逐 cascade 可见数 vs shadow 命令环 instanceCount 合计对照（5.2 cull_stats 同款 BRP 读数对照法），零 mismatch。
4. **性能成组记录**：gpu_shadow_ms 进 5.5 底座七位区间；与 8.2.5 基线对照零异常回退。
5. **零 VUID、WM_CLOSE exit 0、clippy 全净**+ 既有判定线零回退（五脚本 + 库测试 + 4.x 零泄漏 + 5.3 draw_calls=1 口径）。

## 3. 剩余待决（施工时钉明）

| 待决 | 说明 |
|---|---|
| 分割比例与 far 上限 | log 分割初值（4 级 0.5→80m），级联边界读图后调；Sponza 尺度优先 |
| blocker search 核尺寸 | 起步 3×3 + 16 tap PCF；读图噪点再调，不追求最优 |
| shadow map 尺寸 | 2048² 起步；p95 性能与边界读图不满意再评估（改尺寸=重建附件族，随 resize 闸门） |
| texel snap | 防影闪的级联矩阵 texel 对齐——先不做（静态相机 A/B 不闪即够判定线），相机漫游闪影再补 |

## 4. 测试方案

- **BRP 优先**：shadow_set（开关）+ shadow_stats（逐 cascade 可见数/命令计数/计时）新方法族；A/B 与对照由 BRP 驱动，`tools/brp.py` 子命令化。
- **读图**：Sponza 中庭 + 8.2.4 梯度球阵（y=25 投影到屋顶/地面）双场景受控截图（PrintWindow+会话模型读图，画面功能视觉验证规范）；取证进 `.temp/8.3/`。
- **回归矩阵**：hiz/lod/stream/fine/sponza 五脚本、库测试、零 VUID、WM_CLOSE exit 0、clippy 全净、4.x 零泄漏、5.3 draw_calls 口径。

## 5. 边界

- **不主张**：点光/聚光 shadow（LightRecord bit0 记账不消费）、VSM/EVSM（外置条目 7）、时域滤波/TAA 式影稳定、透明层接影（透明 accumulate 不采样 shadow——方向光项只在 opaque 光照累积）。
- **GPU-driven 红线**：shadow pass 剔除/决策全在 GPU（标记+命令环），CPU 只发矩阵与开关；draw_calls 维持 5.3 口径不回退。
- 依赖顺序：8.2 收官（已收官）→ 8.3 → 8.4。
