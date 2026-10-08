# 施工 3.7：egui 调试 GUI

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 收官后追加段（用户 2026-09-29 增补）。主产出为[施工计划](3.7-施工计划：输入桥、图集镜像与overlay管线.md)——egui 裸接 ash 的四件架构、API 契约判据（egui 0.36.2 / egui-wgpu 0.36.2 源码逐项钉死）与同步对账。

**状态：✅ 已收官（2026-10-08）。**四任务全过（3.7.1 状态与输入桥 → 3.7.2 图集与顶点资源 → 3.7.3 overlay 管线与同实例接画 → 3.7.4 回归收官：判定线五条全过 + deepseek-flash 视觉验证）；本段修复一个真 bug = push 双域失配（screen_size 给逻辑点致 UI ×ppp²，`UiPaint::screen_px` 定案，详见[3.7.4](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md) §1）。

讲解篇[《egui裸接ash讲解：输入桥、pass与GPU后端（兼frenderer Dear ImGui对照）》](egui裸接ash讲解：输入桥、pass与GPU后端（兼frenderer-Dear-ImGui对照）.md)（2026-10-08）：三件活分工、输入桥按类近似排序、pass 与交互闭环、Context 记忆与图集缓存两问深入；frenderer Dear ImGui 全链对照含 **GUI 顶点通道新侦察发现**（每帧新建 buffer + `RenderBuffer::drop` 全设备等死——此前对照只覆盖上传链）。

**目的**：把研究篇 §11.2 定案的"窗口内调试 UI"立起来——帧统计与渲染参数在窗口里可见可调，为步骤 4 的实例槽/上传票据/驻留账本提供实时观察面；同时是"3.4 后管线验收题"的兑现（管线、顶点缓冲、描述符、blend、纹理图集全用上）。

**路线**：egui 裸接 ash——禁渲染宿主上 `bevy_egui` 挂不上，自己消费 epaint `ClippedPrimitive` 写 ash 管线；字体图集复用既有 Uploader/GpuImage/bindless 链；输入桥走 bevy 输入事件；绘制接在场景之后（同一 dynamic rendering 实例内 LOAD 形态接画）。

## 任务清单

- [x] **3.7.1 状态与输入桥**：`overlay/` 模块（EguiState/EguiFrame/输入桥/调试窗口骨架/RenderMode 资源化），零 Vulkan；纹理增量就地 clear（过渡，3.7.2 折进镜像）。收账：shapes 非空 + 图集增量 set 1/free 0、稳态零 panic（`TexturesDelta` Drop 审查已抓一次现行并修复）、clippy 净、WM_CLOSE exit 0。
- [x] **3.7.2 图集与顶点资源**：`BufferRole::DynamicDraw` + 每帧槽 UI 顶点环 + 图集 CPU 镜像整图重传新槽（graveyard 与表同寿）+ `paint_overlay` 产 `UiPaint`（只产不画，接画归 3.7.3）。收账：图集整传 14 次（0.85s 内 13 代字形预热 + 1 次迟发，随后稳态零批）、纹理槽 6→29/1024 且图集采样器与 fallback 同键去重落 0 号槽、paint_overlay 收账 draws 3（顶点 658/索引 2223）、零 VUID（含拆 Device）、3.5 判定线零回退、WM_CLOSE exit 0（拆除序"帧池/UI 顶点环 → … → 图集 graveyard"日志核对）。
- [x] **3.7.3 overlay 管线与同实例接画**：第二 GraphicsPipeline（`vulkan/overlay_pipeline.rs`：set0 共享常驻表、push 16B 三方镜像、blend 照 egui-wgpu 0.36.2 判据、深度格式声明对齐读写关）+ `overlay_draw.wgsl` + record_frame UI 段（管线切换→重绑 set0→逐 clip push+scissor+draw_indexed）+ init/teardown 接线。两笔代码订正随段入档：**push screen_size 必须与 tessellate 顶点同域=物理像素**（tessellate 已按 ppp 把 points 换算到像素，传逻辑 points 会让 NDC 再乘一次 scale → UI ×ppp²、点击全部错位——首版实踩，`UiPaint::screen_px` 定案）；输入桥指针类先于按键类（同帧"移动+按下"的按下事件带上本帧新位置）。收账：真实点击切换着色模式三模式全实证（lambert/unlit/normal——单选高亮位移 + 头盔区像素变化 + unlit 暗部提亮 ×4.2 签名）、零 VUID 含拆 Device、3.5 判定线零回退、WM_CLOSE exit 0（拆除序含 overlay 管线/UI 顶点环/图集 graveyard）。
- [x] **3.7.4 回归与收官**：[回归收官施工记录](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md)——判定线 §0 五条全过（三模式真实点击、拖动 +100pt、resize×2、最小化/还原状态保持、WM_CLOSE exit 0×3、三实例零 VUID 含拆 Device、3.5 判定线零回退、图集稳态无泄漏）；deepseek-flash 三轮视觉验证按 `.agents/rules/画面功能视觉验证.md` 规则执行（主交付对通过，两轮误报经放大取证+像素签名仲裁排除）；计划两处失真（push 逻辑点、不声明深度附件）已加订正指针。**3.7 全段收官（2026-10-08）**。

## 判定线

见[施工计划 §0](3.7-施工计划：输入桥、图集镜像与overlay管线.md)：overlay 可见可交互；零 VUID（含退出）；既有同步闸门与帧循环零干扰（3.5 判定线零回退）；resize/最小化/WM_CLOSE 回归；blend 以 egui 官方 wgpu 后端源码为据。**全过（2026-10-08），逐条证据见[3.7.4 回归收官](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md) §0。**

## 待决问题

- 无（图集更新走"CPU 镜像整图重传 + 新槽 + graveyard"路线的取舍见施工计划 §3.1；运行时槽回收与局部区拷贝归步骤 4 及以后）。
