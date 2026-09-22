# 第三步：静态取数链路——glTF → Query → bindless 画出来（M2）（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 第三步，本文件夹存放该步的全部产出。

**目的**：打通入口篇"渲染前准备"第 1 层（bindless 常驻池）的静态版本：spawn 一个 glTF 场景 → 渲染系统 Query 采集 → 上传 ash buffer/VkImage → descriptor indexing（bindless 起步形态）→ 画出来。

**完成标准**：与 bevy 自带 wgpu 渲染同屏对照，几何、贴图、光照方向一致。

**状态：🚧 开工（2026-09-22）**——施工计划已定稿，按"一次一段、理解后再走"的节奏推进（见施工计划 §5 节奏约定）。

## 施工顺序（对应施工计划五段，每段一个子文件夹存施工文档）

| 段 | 文件夹 | 内容 | 状态 |
|---|---|---|---|
| ① | [施工①-ECS侧取数/](施工①-ECS侧取数/README.md) | 材质缝接线（自写 GltfExtensionHandler）+ glTF 场景/相机/灯光 spawn + PostUpdate 采集系统，零 Vulkan 代码 | ⬜ |
| ② | [施工②-buffer侧上传/](施工②-buffer侧上传/README.md) | Context 扩展（transfer 队列 + Vulkan12 特性）+ 顶点/索引大池 + 合批 staging 上传 + timeline 信号量 | ⬜ |
| ③ | [施工③-贴图与bindless描述符/](施工③-贴图与bindless描述符/README.md) | VkImage 上传 + 采样器 + descriptor indexing 全套（UPDATE_AFTER_BIND / PARTIALLY_BOUND / nonuniform）+ 描述符原理篇 | ⬜ |
| ④ | [施工④-管线与绘制/](施工④-管线与绘制/README.md) | WGSL 着色器（naga→SPIR-V）+ 图形管线 + 深度缓冲 + 帧循环从清屏长成绘制 | ⬜ |
| ⑤ | [施工⑤-光照与同屏对照/](施工⑤-光照与同屏对照/README.md) | 方向光/环境光进 UBO + 与 bevy wgpu 同屏对照 + 收官文档 | ⬜ |

每段文件夹的 README = 该段任务面板（目的/任务清单/验证/待决问题）；讲解与踩坑记录随施工增补进各文件夹。推进节奏见施工计划 §5：一次一段，用户理解确认后再走下一段。

## 材料清单

- 《[施工计划：Bindless起步五段拆解.md](施工计划：Bindless起步五段拆解.md)》——主计划（2026-09-22）：bind 模型痛点 → descriptor indexing 四件套入门、终态架构决策表、五段拆解、节奏约定、0.20.0-dev 已核实事实清单
- （施工进行中陆续增补）

## 待决问题

- [ ] 测试资产：FlightHelmet（`assets/models/FlightHelmet/`，4 材质 5 贴图，经典 PBR 对照模型）——是否够用，施工④看到画面再定
- [ ] 同屏对照载体：临时跑 bevy 官方 wgpu 示例 vs 本 crate，具体形态施工⑤定
