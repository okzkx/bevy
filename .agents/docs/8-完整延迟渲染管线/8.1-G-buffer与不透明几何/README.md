# 8.1 G-buffer 与不透明几何（M7 首段）

对应[步骤 8 README](../README.md)与[路线图步骤 8 节](../../学习目标实现步骤.md)。

**目的**：步骤 8 的底座段——把不透明几何从"直出颜色"改成"写 G-buffer"、把透明拆成带分簇光源列表的 Clustered Forward 前向层。G-buffer 三原料（albedo/normal/世界位置）从本段起就是 8.2 光照累积与步骤 9 SCGI 播种/gather 的取数面。

**一句话说明**：本段把不透明几何从"直出颜色"改成"写 G-buffer"、透明拆成开工即 3D cluster 的 Clustered Forward 层并配 GPU bitonic 透明排序、为 motion vectors 附件留位——为 8.2 光照累积与步骤 9 SCGI 备好原料；本段不主张光照模型变化（仍单方向光对照）与任何画质提升，motion vectors 只留位不写内容（本体归步骤 9.x 前置施工件）。

**当前状态：✅ 收官（2026-10-22 五任务全链施工：迁移 → G-buffer 多 pass → 深度重建 → 透明 Clustered Forward → bitonic 排序 → 总验收；判定线四条全过，详见[8.1.5 施工记录](8.1.5-施工记录：判定线总验收与计量.md)；待并回拍板）**。详见[施工计划：G-buffer与透明层拆解](施工计划：G-buffer与透明层拆解.md)。

## 代码落点

- **`render/pipeline/`**（2026-10-22 用户钉明，已落地）：`vulkan/` 20 文件已全量迁移入 `render/{assets,gpu,pipeline}`（git mv 保历史，c80b87c56），`vulkan/` 退役；8.1 新增 gbuffer.rs / fullscreen_pipeline.rs / cluster.rs 与五个 shader（gbuffer_write/composite、present_blit、cluster_build、transparent_collect/gather/sort/forward）。
- 迁移逐文件去向与 shader 子目录钉明见[8.1.1 施工记录](8.1.1-施工记录：render迁移与G-buffer多pass首跑.md)。

## 任务面板

任务拆分与逐任务交付见[施工计划 §1](施工计划：G-buffer与透明层拆解.md)；判定线草案与剩余待决见同文 §2/§3。

| 任务 | 内容 | 状态 |
|---|---|---|
| 8.1.1 | render/ 全量纯移动迁移 + G-buffer 附件分配 + 中间目标生命周期 + 多 color 附件首跑 | ✅ 收官（[施工记录](8.1.1-施工记录：render迁移与G-buffer多pass首跑.md)） |
| 8.1.2 | 不透明 deferred 写入（深度重建世界位置、normal 编码钉明、motion vectors 留位） | ✅ 收官（[施工记录](8.1.2-施工记录：深度反投影重建世界位置.md)） |
| 8.1.3 | 透明 Clustered Forward 层（3D cluster 光源列表 + 透明前向绘制 + 深度复用形态钉明） | ✅ 收官（[施工记录](8.1.3-施工记录：透明ClusteredForward层.md)） |
| 8.1.4 | GPU bitonic 透明排序 → indirect draw | ✅ 收官（[施工记录](8.1.4-施工记录：GPUbitonic透明排序.md)） |
| 8.1.5 | 判定线总验收与计量 | ✅ 收官（[施工记录](8.1.5-施工记录：判定线总验收与计量.md)） |

## 材料清单

- [功能流程导读：G-buffer与透明层的帧流走读](8.1-功能流程导读：G-buffer与透明层的帧流走读.md)——多 pass 帧编排的逐时刻走读（新读者入口）。
- [施工计划：G-buffer与透明层拆解](施工计划：G-buffer与透明层拆解.md)——2026-10-22 立案：两枚裁决（世界位置=深度重建、render/ 全量一次迁移）、任务五项、判定线四条、剩余待决六项（全部钉明，落点见各施工记录）。
- [施工计划：完整延迟渲染管线五段拆解](../施工计划：完整延迟渲染管线五段拆解.md)——8.1 交付/前置闸门/判定线草案（§1）、待决项汇总（§2）、SCGI 前置接口（§3）、目录组织定案（§5）。
- [体素渲染调研：VXGI 的大场景证伪与光源分簇的可行条件](../../笔记/渲染/体素渲染调研：VXGI%20的大场景证伪与光源分簇的可行条件.md)——3D cluster 光源分簇参照。
