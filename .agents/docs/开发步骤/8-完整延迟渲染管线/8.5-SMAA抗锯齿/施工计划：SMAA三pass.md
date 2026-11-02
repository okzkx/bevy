# 施工计划：SMAA 三 pass（8.5 段）

> **立案补档（2026-10-23，as-built）**：本段实际施工先于计划成文（2026-10-22 随 `$plan-execute 步骤八` 连续施工，当日运行级验证未过、分支 `goal/8.5` 留存续工；2026-10-23 续工修复后判定线全过）。本文按最终落地形状补档立案，全部"拍板"即已实施的决策。

## 一句话说明

本段 tonemap 后接 SMAA 三 pass（边缘检测/权重计算/邻域混合），作为延迟管线的抗锯齿标准搭档收口步骤 8；本段不主张 TAA 与任何时域手段。

## 0. 定位

8.4 收官后帧链为 `HDR 合成 → Bloom → tonemap → swapchain 直出`，画面无抗锯齿（延迟管线不用 MSAA）。8.5 在 tonemap 输出与 swapchain 直出之间插入 SMAA 官方 1x 三 pass，是步骤 8 的收口段。判定线以[步骤级施工计划 §1 8.5 节](../施工计划：完整延迟渲染管线五段拆解.md)为准。

## 1. 技术决策（施工时钉明）

| 待决 | 裁决 | 依据 |
|---|---|---|
| LUT 来源 | **官方预生成 texture**：bevy_anti_alias 内置 ktx2（zstd 压缩）→ PNG 转档嵌入（`_smaa_area_lut.png` 160×560 / `_smaa_search_lut.png` 64×16），`Image::from_buffer` 解码 → upload 直传 GpuImage | 运行时计算 LUT 的工程量与收益不成比例；官方预生成是标准做法 |
| 前端源 | bevy_anti_alias `smaa.wesl` @ 0.20-dev（MIT，官方移植）拆分器行号提取 + 条件编译展开（HIGH 档、corner/diag 全开、无 predication/reprojection） | 上游版权头保留；拆分器脚本 `.temp/8.5/split_smaa.py` |
| 链路落点 | tonemap 落点从 swapchain 改 **LDR 中间目标**（R8G8B8A8_UNORM 线性存储）→ edge → blend → neighborhood（读 LDR+blend → swapchain，UI 同实例接画） | 线性域抗锯齿（官方"非 sRGB 读写"条款的线性管线实现：全链无编码，编码仍在终点 view 恰好一次——3.4 拍板维持） |
| 绑定形状 | set0 共享五槽：b0 输入纹理 / b1 SmaaInfo / b2 采样器 / b3+b4 LUT 或 blend；neighborhood 加 flags.x 开关直通 | 三 pass 共一套布局族（a/b 两版），描述符三组 ×帧槽 |
| 帧槽纪律 | LDR/edges/blend 三目标 ×MAX_FRAMES_IN_FLIGHT 环 + SmaaInfo UBO 帧槽环；目标进出屏障恒录（UNDEFINED 进 = 内容可弃） | 照 bloom 环化先例；两帧在飞布局竞争 |
| 开关 | BRP `smaa_set`（tools/brp.py `smaa-set`）；关 = edge/blend 跳过、neighborhood 权重清零直通（画面回 8.4 基线） | A/B 判定线驱动面 |

## 2. 任务拆解

| 任务 | 内容 | 交付 |
|---|---|---|
| 8.5.1 三 pass shader | 官方 smaa.wesl 拆分为 smaa_edge/smaa_blend/smaa_neighborhood 三文件，入口 vs_main/fs_main | `shaders/smaa_*.wgsl`（build.rs naga+spirv-val 闸门在链） |
| 8.5.2 资源与管线 | SmaaTargets（三目标环 + LUT 直传 + SmaaInfo UBO + 三组描述符集）+ SmaaPipelines（三全屏管线，本体持有——8.4 句柄拷贝陷阱先例） | `render/pipeline/smaa.rs` |
| 8.5.3 帧链接线 | tonemap 落点改 LDR → edge/blend 条件录（smaa 开 = Some）→ neighborhood 终点（swapchain + UI）；resize 闸门③纳入 smaa.rebuild | `render/pipeline/frames.rs` + `driver/host/frame.rs` |
| 8.5.4 判定线与回归 | smaa_set A/B 读图、连拍、性能成组、回归矩阵 | [施工记录](8.5-施工记录：SMAA三pass.md) |

## 3. 判定线（照步骤级 §1 8.5 节）

1. 几何边缘锯齿读图对照（开关 A/B）；
2. 无时域类伪影（SMAA 非时域——静止与相机移动两种机位连拍）；
3. 性能成本成组记录；
4. 零 VUID、WM_CLOSE exit 0、clippy 全净（+库测试、回归五脚本、draw_calls 红线）。

## 4. 边界

不主张 TAA/任何时域手段（8.5 是纯空域抗锯齿）；SMAA 不处理透明层的几何锯齿以外的问题（alpha 边缘归内容）；不升级 DLAA/FXAA 对照面。
