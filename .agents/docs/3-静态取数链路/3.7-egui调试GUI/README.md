# 施工 3.7：egui 调试 GUI

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 收官后追加段（用户 2026-09-29 增补）。主产出为[施工计划](3.7-施工计划：输入桥、图集镜像与overlay管线.md)——egui 裸接 ash 的四件架构、API 契约判据（egui 0.36.2 / egui-wgpu 0.36.2 源码逐项钉死）与同步对账。

**目的**：把研究篇 §11.2 定案的"窗口内调试 UI"立起来——帧统计与渲染参数在窗口里可见可调，为步骤 4 的实例槽/上传票据/驻留账本提供实时观察面；同时是"3.4 后管线验收题"的兑现（管线、顶点缓冲、描述符、blend、纹理图集全用上）。

**路线**：egui 裸接 ash——禁渲染宿主上 `bevy_egui` 挂不上，自己消费 epaint `ClippedPrimitive` 写 ash 管线；字体图集复用既有 Uploader/GpuImage/bindless 链；输入桥走 bevy 输入事件；绘制接在场景之后（同一 dynamic rendering 实例内 LOAD 形态接画）。

## 任务清单

- [x] **3.7.1 状态与输入桥**：`overlay/` 模块（EguiState/EguiFrame/输入桥/调试窗口骨架/RenderMode 资源化），零 Vulkan；纹理增量就地 clear（过渡，3.7.2 折进镜像）。收账：shapes 非空 + 图集增量 set 1/free 0、稳态零 panic（`TexturesDelta` Drop 审查已抓一次现行并修复）、clippy 净、WM_CLOSE exit 0。
- [x] **3.7.2 图集与顶点资源**：`BufferRole::DynamicDraw` + 每帧槽 UI 顶点环 + 图集 CPU 镜像整图重传新槽（graveyard 与表同寿）+ `paint_overlay` 产 `UiPaint`（只产不画，接画归 3.7.3）。收账：图集整传 14 次（0.85s 内 13 代字形预热 + 1 次迟发，随后稳态零批）、纹理槽 6→29/1024 且图集采样器与 fallback 同键去重落 0 号槽、paint_overlay 收账 draws 3（顶点 658/索引 2223）、零 VUID（含拆 Device）、3.5 判定线零回退、WM_CLOSE exit 0（拆除序"帧池/UI 顶点环 → … → 图集 graveyard"日志核对）。
- [ ] **3.7.3 overlay 管线与同实例接画**：第二 GraphicsPipeline + `overlay_draw.wgsl` + record_frame UI 段 + init/teardown 接线 + 调试窗口内容。
- [ ] **3.7.4 回归与收官**：判定线全量回归 + 施工记录。

## 判定线

见[施工计划 §0](3.7-施工计划：输入桥、图集镜像与overlay管线.md)：overlay 可见可交互；零 VUID（含退出）；既有同步闸门与帧循环零干扰（3.5 判定线零回退）；resize/最小化/WM_CLOSE 回归；blend 以 egui 官方 wgpu 后端源码为据。

## 待决问题

- 无（图集更新走"CPU 镜像整图重传 + 新槽 + graveyard"路线的取舍见施工计划 §3.1；运行时槽回收与局部区拷贝归步骤 4 及以后）。
