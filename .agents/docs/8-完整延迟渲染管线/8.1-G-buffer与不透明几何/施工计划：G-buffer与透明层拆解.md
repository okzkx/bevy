# 施工计划：8.1 G-buffer 与透明层拆解

> 2026-10-22 立案（用户指令，纯文档立案）。对应[步骤级施工计划 §1 8.1 节](../施工计划：完整延迟渲染管线五段拆解.md)与[段 README](README.md)：把不透明几何从"直出颜色"改成"写 G-buffer"、把透明拆成 Clustered Forward 前向层。本文管 8.1 段做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。

**一句话说明**：本段把不透明几何从"直出颜色"改成"写 G-buffer"（世界位置走深度重建）、透明拆成开工即 3D cluster 的 Clustered Forward 层并配 GPU bitonic 透明排序、motion vectors 附件只留位——为 8.2 光照累积与步骤 9 SCGI 备好 G-buffer 三原料；本段不主张光照模型变化（仍单方向光 A/B 对照）与任何画质提升，代码全部落 `render/pipeline/`。

## 0. 立案期裁决（2026-10-22 用户拍板）

- **世界位置取法=深度重建**：从深度附件反投影（clip→world）重建世界位置，不占独立位置附件。省一条全屏附件（约 12B/px）与带宽，行业默认（Unity/UE deferred 同款）；6.1 反向 Z + D32 在本项目场景规模下远景重建误差对 SCGI patch 粒度不敏感；调试期可临时加位置附件做 BRP 读数，不进正式布局。
  - **连带发现（立案期核实）**：重建需要 inv_view_proj（clip→world）进每帧常量；现 set1 b0 FrameUniforms 为 128B 冻结版（view_proj 64B + 灯光 64B，属实 `vulkan/descriptors.rs:48`）——加 64B 逆矩阵必解冻一次。与 8.2 光源扩容的解冻合并还是分开，施工时钉明（照 5.1.1 解冻清单先例）。
- **迁移范围=全量一次迁移**：8.1 开工先做一次纯移动迁移（git mv 保历史，3.6 整理与 overlay 分层先例），`vulkan/` 全部 20 个现役文件（属实清点 2026-10-22：context/cull/depth/descriptors/frames/hiz/images/indirect/instance_table/lod/mesh_convert/mod/occlusion/overlay_pipeline/perf/pipeline/pool/resources/swapchain/uploader）按职责归入 `render/` 对应子目录，`vulkan/` 目录退役；逐文件去向施工时按步骤级施工计划 §5 提案形状钉明。8.1 全部代码（迁移后文件的改动 + 新增）写在 `render/pipeline/`。

## 1. 任务拆分

| 任务 | 交付 | 状态 |
|---|---|---|
| 8.1.1 | render/ 全量纯移动迁移（零行为变化）+ G-buffer 附件分配（albedo/normal/深度）+ 渲染目标从 swapchain 直出改中间目标的 resize/最小化生命周期 + 多 color 附件渲染首跑 | ⬜ |
| 8.1.2 | 不透明 deferred 写入：G-buffer 写入 shader；深度反投影重建世界位置（inv_view_proj 常量面扩容）；normal 编码钉明（世界空间直存 vs 八面体压缩，带宽账与取数便利取舍）；motion vectors 附件留位不写内容 | ⬜ |
| 8.1.3 | 透明 Clustered Forward 层：开工即 3D cluster（视锥 AABB 分簇）光源列表 compute + 逐 tile×深度片光源列表；透明前向绘制 pass；不透明深度对透明的复用形态钉明（附件重绑 vs 取数通道，6.1 已实证深度附件不可采样） | ⬜ |
| 8.1.4 | GPU bitonic 透明排序：按深度键排 draw 索引的 sort compute → indirect draw，消掉最后一个逐对象 CPU 决策残留 | ⬜ |
| 8.1.5 | 判定线总验收与计量：resize/最小化回归总跑、性能成组记录（G-buffer 带宽随分辨率不随实例数，5.5 计量底座复用） | ⬜ |

## 2. 判定线草案（段级）

1. **G-buffer 各附件内容与现行 forward 输出语义逐项对齐**（单光源 A/B 对照，读图）。
2. **透明层与不透明主体同场景光照一致**；透明层光源列表与 CPU 参照逐 tile 对照零 mismatch（5.2 cull_stats 口径沿用）。
3. **GPU 排序结果与 CPU 深度排序参照一致**（物体级）。
4. **resize/最小化回归、零 VUID、WM_CLOSE exit 0、clippy 全净**；GPU-driven 闭环不回退（draw_calls 维持 5.3 口径）。

## 3. 剩余待决（施工时钉明）

| 待决 | 说明 |
|---|---|
| normal 编码 | 世界空间 xyz 直存 vs 八面体压缩——带宽账与取数便利取舍 |
| set1 128B 解冻时机 | 8.1 inv_view_proj 扩容与 8.2 光源扩容合并解冻还是分开 |
| 透明层深度来源 | 不透明深度对透明 pass 的复用形态（附件重绑 vs 取数通道） |
| HDR 中间格式 blending 支持 | R16G16B16A16_SFLOAT 的 blending 支持待实测（步骤级前置闸门） |
| 迁移逐文件去向 | vulkan/ 20 文件归 render/ 哪个子目录（§5 提案形状为准） |
| shader 子目录 | examples/shaders/ 是否按 pass 分子目录 |

## 4. 边界

- **不主张光照模型变化**：仍单方向光 Lambert 对照，材质参数面与 BRDF 归 8.2。
- **不主张画质提升**：G-buffer 语义与现行 forward 对齐是本段验收线，"变好看"不是。
- **精确 OIT 不主张**（[外置拓展](../../外置拓展需求.md)）；motion vectors 本体归步骤 9.x 前置施工件。
- **GPU-driven 闭环不回退是红线**：加 pass 不许把绘制决策搬回 CPU。
- 依赖顺序：本段是 8.2→8.5 的底座，段内 8.1.1 迁移与布局先行；开工前置=步骤 5/6 步骤级验收归档欠账（[路线图当前位置](../../学习目标实现步骤.md)）。
