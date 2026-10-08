# 施工 3.7：egui 调试 GUI

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 收官后追加段（用户 2026-09-29 增补）。主产出为[施工计划](3.7-施工计划：输入桥、图集镜像与overlay管线.md)——egui 裸接 ash 的四件架构、API 契约判据（egui 0.36.2 / egui-wgpu 0.36.2 源码逐项钉死）与同步对账。

**状态：✅ 已收官（2026-10-08），收官后一笔点击偏移订正（3.7.5）。**四任务全过（3.7.1 状态与输入桥 → 3.7.2 图集与顶点资源 → 3.7.3 overlay 管线与同实例接画 → 3.7.4 回归收官：判定线五条全过 + deepseek-flash 视觉验证）；~~本段修复一个真 bug = push 双域失配~~（**已被 3.7.5 再订正**：tessellate 顶点是 points 域，3.7.3 的"物理域定案"才是引入点击偏移的错刀，详见[3.7.4](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md) §7）。

讲解篇[《egui裸接ash讲解：输入桥、pass与GPU后端（兼frenderer-Dear-ImGui对照）》](egui裸接ash讲解：输入桥、pass与GPU后端（兼frenderer-Dear-ImGui对照）.md)（2026-10-08）：三件活分工、输入桥按类近似排序、pass 与交互闭环、Context 记忆与图集缓存两问深入；frenderer Dear ImGui 全链对照含 **GUI 顶点通道新侦察发现**（每帧新建 buffer + `RenderBuffer::drop` 全设备等死——此前对照只覆盖上传链）。

订正讲解篇[《UI点击偏移订正讲解：tessellate顶点域、push尺寸域与DPI测量假阳性》](UI点击偏移订正讲解：tessellate顶点域、push尺寸域与DPI测量假阳性.md)（2026-10-08，随 3.7.5）：点击偏移 bug 的完整因果链——tessellate 顶点=points 域的源码判据、三种 push 域对照表（两种错法方向相反）、DPI 不感知测量两把尺子互相抵消的假阳性机制、探针色块/点击三域对账方法论。

**目的**：把研究篇 §11.2 定案的"窗口内调试 UI"立起来——帧统计与渲染参数在窗口里可见可调，为步骤 4 的实例槽/上传票据/驻留账本提供实时观察面；同时是"3.4 后管线验收题"的兑现（管线、顶点缓冲、描述符、blend、纹理图集全用上）。

**路线**：egui 裸接 ash——禁渲染宿主上 `bevy_egui` 挂不上，自己消费 epaint `ClippedPrimitive` 写 ash 管线；字体图集复用既有 Uploader/GpuImage/bindless 链；输入桥走 bevy 输入事件；绘制接在场景之后（同一 dynamic rendering 实例内 LOAD 形态接画）。

## 任务清单

- [x] **3.7.1 状态与输入桥**：`overlay/` 模块（EguiState/EguiFrame/输入桥/调试窗口骨架/RenderMode 资源化），零 Vulkan；纹理增量就地 clear（过渡，3.7.2 折进镜像）。收账：shapes 非空 + 图集增量 set 1/free 0、稳态零 panic（`TexturesDelta` Drop 审查已抓一次现行并修复）、clippy 净、WM_CLOSE exit 0。
- [x] **3.7.2 图集与顶点资源**：`BufferRole::DynamicDraw` + 每帧槽 UI 顶点环 + 图集 CPU 镜像整图重传新槽（graveyard 与表同寿）+ `paint_overlay` 产 `UiPaint`（只产不画，接画归 3.7.3）。收账：图集整传 14 次（0.85s 内 13 代字形预热 + 1 次迟发，随后稳态零批）、纹理槽 6→29/1024 且图集采样器与 fallback 同键去重落 0 号槽、paint_overlay 收账 draws 3（顶点 658/索引 2223）、零 VUID（含拆 Device）、3.5 判定线零回退、WM_CLOSE exit 0（拆除序"帧池/UI 顶点环 → … → 图集 graveyard"日志核对）。
- [x] **3.7.3 overlay 管线与同实例接画**：第二 GraphicsPipeline（`vulkan/overlay_pipeline.rs`：set0 共享常驻表、push 16B 三方镜像、blend 照 egui-wgpu 0.36.2 判据、深度格式声明对齐读写关）+ `overlay_draw.wgsl` + record_frame UI 段（管线切换→重绑 set0→逐 clip push+scissor+draw_indexed）+ init/teardown 接线。两笔代码订正随段入档：~~push screen_size 必须与 tessellate 顶点同域=物理像素~~（**已被 3.7.5 再订正**：tessellate 顶点是 points 域，首版 points 实现本就正确，物理域 push 才是错刀）；输入桥指针类先于按键类（同帧"移动+按下"的按下事件带上本帧新位置）。收账：真实点击切换着色模式三模式全实证（lambert/unlit/normal——单选高亮位移 + 头盔区像素变化 + unlit 暗部提亮 ×4.2 签名）、零 VUID 含拆 Device、3.5 判定线零回退、WM_CLOSE exit 0（拆除序含 overlay 管线/UI 顶点环/图集 graveyard）。
- [x] **3.7.4 回归与收官**：[回归收官施工记录](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md)——判定线 §0 五条全过（三模式真实点击、拖动 +100pt、resize×2、最小化/还原状态保持、WM_CLOSE exit 0×3、三实例零 VUID 含拆 Device、3.5 判定线零回退、图集稳态无泄漏）；deepseek-flash 三轮视觉验证按当时 `.agents/rules/画面功能视觉验证.md` 规则执行（主交付对通过，两轮误报经放大取证+像素签名仲裁排除；**该规则 3.7.5 当日已修订为当前模型直接读图**）；计划失真（不声明深度附件）已加订正指针。**3.7 全段收官（2026-10-08）**。
- [x] **3.7.5 点击偏移订正（收官后 bugfix，记录在 3.7.4 文档 §7）**：用户实测"点击要在按钮下方才点得到、之前某版本正常"——三重证据（epaint tessellator 源码 / egui-wgpu 0.36.2 uniform 字段 `screen_size_in_points` / DPI 感知像素实测）定案 tessellate 顶点=POINTS 域，3.7.3 的"物理域定案"系误诊（其测量跑在 DPI 不感知进程里，量与注入两把虚拟尺子互相抵消成假阳性）。修复 = `UiPaint::screen_pt`（swapchain extent ÷ ppp）贯穿 paint→frames→push + 三处失真注释订正；回归全量绿（探针色块 (875,375)/125×62.5 精确命中、注入+用户真实鼠标点击均命中、clippy 净、零 VUID、判定线零回退、resize/最小化闸门、WM_CLOSE exit 0、当前模型读图验证通过）。**视觉验证规则同日修订：deepseek-flash 外部路线退役（当日连续 503），改当前模型直接读图。**

## 判定线

见[施工计划 §0](3.7-施工计划：输入桥、图集镜像与overlay管线.md)：overlay 可见可交互；零 VUID（含退出）；既有同步闸门与帧循环零干扰（3.5 判定线零回退）；resize/最小化/WM_CLOSE 回归；blend 以 egui 官方 wgpu 后端源码为据。**全过（2026-10-08），逐条证据见[3.7.4 回归收官](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md) §0。**

## 待决问题

- 无（图集更新走"CPU 镜像整图重传 + 新槽 + graveyard"路线的取舍见施工计划 §3.1；运行时槽回收与局部区拷贝归步骤 4 及以后）。
