# 施工 3.4：管线与绘制——从清屏长成画场景

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。

**目的**：第一次看到 FlightHelmet——着色器、图形管线、深度测试、绘制命令全链落地。

**状态：⬜ 未开工**

## 任务清单

- [ ] **3.4.1 WGSL 两支**：vert（顶点变换：模型矩阵 × view_proj）+ frag（`nonuniformEXT` 取贴图数组 + 简单光照）；naga 运行时编译 SPIR-V（启动期一次；失败走 Tier② 冒泡优雅退出）；
- [ ] **3.4.2 图形管线**：dynamic rendering（颜色附件 + 深度附件）、动态 viewport/scissor、顶点输入布局对齐交错格式、背面剔除先关（验证绕向后开）、深度 LESS + 写入；
- [ ] **3.4.3 深度缓冲进 Swapchain**：D32_SFLOAT（查格式支持），随 swapchain 重建建/拆（resize 级寿命，归 Swapchain 管——步骤 2 分家清单补员）；
- [ ] **3.4.4 `record_clear_and_submit` → `record_frame`**：上传 flush → 清屏 → 逐 draw（push constants：model / tex_index / base_color）；同步骨架（2 帧在飞 + 双信号量 + fence）不动；
- [ ] **3.4.5 DrawList 接进录制段**（值拷贝跨帧，见施工计划 §2 图）。

## 验证（本段完成标准）

1. 头盔出现在窗口，几何完整无破面；
2. resize / 退出链路复测不回退（步骤 2 行为保留）；
3. 帧循环编排（draw_frame）结构不动，生长只在录制段——步骤 2《搭建记录》§6 接口承诺兑现。

## 材料清单

- （施工进行中陆续增补）

## 待决问题

- naga SPIR-V 输出的 feature 开关（capability 注入方式）施工中定；
- gamma 责任：贴图 SRGB view 线性化 + 输出端 gamma 编码方式（pow vs sRGB view）施工中定。
