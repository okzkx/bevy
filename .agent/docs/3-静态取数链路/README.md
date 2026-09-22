# 步骤 3：静态取数链路——glTF → Query → bindless 画出来（M2）（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 步骤 3，本文件夹存放该步的全部产出。主计划（`施工计划：….md`）与 README 留根，施工段各自成文件夹。（编号约定：段 = 3.1~3.5，任务 = 3.x.y，见路线图"编号约定"）

**目的**：打通入口篇"渲染前准备"第 1 层（bindless 常驻池）的静态版本：实例化一个 glTF WorldAsset → 渲染系统 Query 采集 → 上传 ash buffer/VkImage → descriptor indexing（bindless 起步形态）→ 画出来。

**完成标准**：与 bevy 自带 wgpu 渲染同屏对照，几何、贴图、光照方向一致。

**状态：🚧 开工（2026-09-22）**——施工计划已定稿，按"一次一段、理解后再走"的节奏推进（见施工计划 §5 节奏约定）。

## 施工顺序（对应施工计划五段，每段一个子文件夹存施工文档）

| 段 | 文件夹 | 内容 | 状态 |
|---|---|---|---|
| 3.1 | [3.1-ECS侧取数/](3.1-ECS侧取数/README.md) | 材质缝接线（自写 GltfExtensionHandler）+ glTF WorldAsset 实例化/相机/灯光 spawn + PostUpdate 采集系统，零 Vulkan 代码 | 🚧 3.1.1–3.1.3 ✅ |
| 3.2 | [3.2-buffer侧上传/](3.2-buffer侧上传/README.md) | Context 扩展（transfer 队列 + Vulkan12 特性）+ 顶点/索引大池 + 合批 staging 上传 + timeline 信号量 | ⬜ |
| 3.3 | [3.3-贴图与bindless描述符/](3.3-贴图与bindless描述符/README.md) | VkImage 上传 + 采样器 + descriptor indexing 全套（UPDATE_AFTER_BIND / PARTIALLY_BOUND / nonuniform）+ 描述符原理篇 | ⬜ |
| 3.4 | [3.4-管线与绘制/](3.4-管线与绘制/README.md) | WGSL 着色器（naga→SPIR-V）+ 图形管线 + 深度缓冲 + 帧循环从清屏长成绘制 | ⬜ |
| 3.5 | [3.5-光照与同屏对照/](3.5-光照与同屏对照/README.md) | 方向光/环境光进 UBO + 与 bevy wgpu 同屏对照 + 收官文档 | ⬜ |

每段文件夹的 README = 该段任务面板（目的/任务清单/验证/待决问题）；讲解与踩坑记录随施工增补进各文件夹。推进节奏见施工计划 §5：一次一段，用户理解确认后再走下一段。

## 材料清单

- 《[施工计划：Bindless起步五段拆解.md](施工计划：Bindless起步五段拆解.md)》——主计划（2026-09-22）：bind 模型痛点 → descriptor indexing 四件套入门、终态架构决策表、五段拆解、节奏约定、0.20.0-dev 已核实事实清单
- 《[代码结构整理：main只做统筹（宿主桥分家）.md](材料/代码结构整理：main只做统筹（宿主桥分家）.md)》——开工整理（2026-09-22，3.1 与 3.2 之间）：main.rs 230 行瘦身为纯插件组装（~50 行、零 Vulkan 符号），init/draw_frame/teardown 三系统迁入 `host.rs` 宿主桥；`draw_frame` 编排与函数名不动；给 3.2~3.5 立边界（新模块自含插件落位、main 冻结）
- 《[代码结构整理：scene拆组与src分层.md](材料/代码结构整理：scene拆组与src分层.md)》——3.1.3 收尾整理（2026-09-22，3.1.4 之前）：scene.rs 按职责五拆成 scene/ 子模块（mod.rs = 组清单 + 重出口，含相机/灯光解耦成组的代码追平）、vulkan/swapchain/frames 归拢 vulkan/（context/swapchain/frames）；main 与宿主桥不动；3.1.4 collect、3.2 资源件照子模块落位
- （施工进行中陆续增补）

## 待决问题

- [ ] 测试资产：FlightHelmet（`assets/models/FlightHelmet/`，6 材质 15 贴图，经典 PBR 对照模型；早版记"4 材质 5 贴图"系笔误，2026-09-22 实测订正）——是否够用，施工 3.4 看到画面再定
- [ ] 同屏对照载体：临时跑 bevy 官方 wgpu 示例 vs 本 crate，具体形态施工 3.5 定
