# egui 裸接 ash 讲解：输入桥、pass 与 GPU 后端（兼 frenderer Dear ImGui 对照）

> 2026-10-08 会话讲解记录（3.7 全段同日收官；内容基于 3.7.1~3.7.3 落地代码讲解，ppp 双域订正已含在内，与收官版一致）。起点三问（egui 怎么通过输入获取鼠标信息 / 获取后做了什么 / 与 Vulkan 怎么交互），追问两题（ctx.pass 是否每帧重新生成 / 字体纹理呢），收于 frenderer Dear ImGui 接线全链对照。本篇纯讲解、无代码改动；§5 含 frenderer GUI 顶点通道的**新侦察发现**（每帧新建 buffer + Drop 时全设备等死）——此前 frenderer 对照只覆盖上传（staging/图集）链，未覆盖此路径。

## §0 判定线

1. **egui 裸接 = 三件活全自己写**：输入桥（egui-winit 的替身）、egui 本体驱动（Context 的 pass）、GPU 后端（egui-wgpu 的替身）。bevy_egui 三件全包但挂不上禁渲染宿主。
2. **立即模式"每帧重建"重建的是声明不是状态**：UI 代码每帧重跑、shapes 每帧全新，但一切要记的东西（窗口拖动位置/布局尺寸/交互状态机/动画）在 Context 里按 widget Id 常驻。
3. **字体图集是驻留缓存，不是每帧产物**：用到才长、长完就定，稳态零上传；每帧真正的重复成本只有 pass 求值、镶嵌、顶点环重写三样，图集不在其中。
4. **与 frenderer Dear ImGui 的实质差异只有两个维度**：数据的时间形状（每帧等死分配 vs 帧槽环 + fence 纪律）与纹理选择权的住所（绑定调用状态 vs push 索引 u32）；架构其余部分两边同构。

## §1 总框架：三件活全自己干

egui 官方生态把"显示一个 egui 窗口"拆成三件：**输入桥**（egui-winit 的活：窗口系统事件 → `RawInput`）、**egui 本体**（Context 的 pass：产形状）、**GPU 后端**（egui-wgpu 的活：镶嵌顶点 → 画）。`bevy_egui` 是三件全包的整合件，但必须挂在 bevy 渲染宿主上——本项目宿主禁渲染，挂不上。所以 3.7 裸接 ash 即三件全自己写：

| 官方件 | 本项目对应 |
|---|---|
| egui-winit | `overlay/input.rs`（输入桥） |
| egui 本体驱动 | `overlay/ui.rs`（状态/字体/pass/调试窗口） |
| egui-wgpu | `overlay/paint.rs`（图集/顶点/UiPaint）+ `vulkan/overlay_pipeline.rs`（管线）+ `vulkan/frames.rs` UI 段（录制） |

跨帧数据流一行：**Update 里 `run_egui_pass`（纯 CPU、零 Vulkan）产 `EguiFrame`（未镶嵌 shapes + ppp + 屏幕尺寸）→ Last 的 `draw_frame` 里 `paint_overlay` 消费它产 `UiPaint` → `record_frame` 的 UI 段接画**。CPU 半边与 GPU 半边只在 `EguiFrame` 这个资源上交接。

## §2 输入桥：egui 怎么拿到鼠标信息

egui 根本不碰窗口系统——它只认识 `RawInput.events` 这一个事件流。鼠标信息真源是 bevy_winit 把 Win32 消息转译进 ECS 消息队列，桥在 Update 里用 `MessageReader` 逐类 drain 再翻译：

- **位置**：`CursorMoved` → `egui::Event::PointerMoved(pos)`。bevy 给的是窗口逻辑像素，恰好就是 egui 的 points 单位，无需换算；同时把 pos 记进 `EguiInput`（跨帧 Resource）的 `pointer_pos`。
- **按键**：bevy 的 `MouseButtonInput` **不带坐标**（bevy 输入的事实坑）——桥从 `pointer_pos` 补上位置发 `PointerButton { pos, button, pressed, modifiers }`；指针不在窗口（`CursorLeft` 已把 `pointer_pos` 清 None）时整条丢弃。
- **滚轮**：`MouseWheel`，Line 单位直通、Pixel 单位除以 ppp 转 points（egui-winit 同款换算）。

两个非显然设计点：

**事件次序只能按类近似。** bevy 各类事件独立缓冲（keyboard、mouse_button、cursor……各有各的队列），跨类没有全局序；而 egui-winit 处理的是按到达序排好的单流。桥按类排序：键盘 → 指针移动/离开 → 按键 → 滚轮。**指针先于按键是刻意的**——同帧"移动+点击"时，按下事件才带得上本帧新位置（否则用上一帧旧位置，首帧更是 None 整条丢）。

**修饰键走快照 + 补发。** egui 0.36 的 `RawInput` 没有顶层 modifiers 字段（源码钉死）——修饰键只随各事件携带。桥给每个事件附上从 `ButtonInput<KeyCode>` 现查的快照，且快照与上帧不同时补发一条 `ModifiersChanged`。

屏幕域方面：`RawInput.screen_rect` = 窗口逻辑尺寸（左上原点 points 域）；ppp 不在 RawInput 里，走 `ctx.set_pixels_per_point`（每帧 pass 前设 = 窗口 scale_factor）。

## §3 egui 拿到输入后做什么：pass 三件套

Update 里的 `run_egui_pass`：

```
ctx.set_pixels_per_point(ppp)
ctx.begin_pass(raw)            ← RawInput 进
    debug_window(&ctx, ...)    ← 立即模式 UI 代码，每帧重建
ctx.end_pass()                 ← FullOutput 出
```

egui 内部把事件流加工成"指针语义"：最新位置、按住状态、click/drag 判定（按下到抬起位移小于阈值才算 click）、hover 命中（pointer_pos 对 widget 矩形做测试）。`debug_window` 里 `ui.selectable_value(mode, RenderMode::Unlit, "unlit")` 是立即模式调用——每帧重跑、本帧立刻求值"这个 widget 被 click 了吗"，click 成立就直接写 `RenderMode` 这个 ECS 资源。**交互闭环不走任何 Vulkan**：UI 的 click 结论 → 改资源 → Last 的 `draw_frame` 读同一个资源写进帧 UBO 的 `mode` 字段 → 同帧提交，头盔就换了着色模式。

`end_pass` 的 `FullOutput` 有两块要消化：

- `shapes`：**未镶嵌**的原始形状（`ClippedShape`，左上原点 points 域）。镶嵌特意不做在 CPU 半边——写顶点环必须过 `wait_for_slot` 的 fence，那是 `draw_frame` 的事。
- `textures_delta`：字体图集增量。在 pass 出口**就地**折进 `AtlasMirror`（CPU 镜像）然后 `clear`——两个原因：egui 的 `TexturesDelta` 带 Drop 审查（不消费就告警）；最小化帧 Update 照跑而 `draw_frame` 整帧让路，增量跨系统存进 EguiFrame 会被下帧覆写丢数据。

字体：egui 自带拉丁（default_fonts）+ `C:\Windows\Fonts\msyh.ttc`（face 0）插 Proportional 回退位管 CJK；读不到 warn 后继续（Tier①，豆腐块不拦帧循环）。

## §4 追问一：pass 是每帧重新生成吗——声明每帧重跑，Context 常驻带记忆

**Context 只建一次**：Startup 里 `init_egui` 建 `EguiState { ctx }`，作为 Resource 活整个进程。每帧重跑的是 pass 这件事本身——`begin_pass(raw) → 建 UI → end_pass()`，pass 的生命周期只覆盖中间那段 UI 构建代码，pass 外不能查 widget，一个 pass 就是"一轮对话"。

**"每帧重建"重建的是声明，不是状态**。立即模式的确切含义是：UI 代码（`debug_window` 那个闭包）每帧从头重跑一遍，每帧产出全新的 shapes 列表。但跨帧要"记住"的东西全在 Context 里，按 widget Id 存：

- 窗口拖动后的位置、折叠态——我们代码里没有任何自己存的状态，全是 Context 记的
- 每个 widget 上一帧的量测尺寸——**布局用上帧尺寸排版**（尺寸变化一帧收敛；egui 命中测试"有一帧布局滞后"的根源）
- 交互状态机（按住的起点位置、click/drag 判定用的计时和位移）
- 动画状态（窗口淡入淡出）

视觉上的连续性不是"每帧重新生成"出来的，是**重跑的声明 + 常驻的记忆**叠出来的——与 Dear ImGui 同一个模型。widget 消失后记忆不会立刻删，egui 延迟若干帧回收。边角：最小化帧 Update 照跑、pass 照常执行——Context 与记忆都在，只有 `draw_frame` 让路；这也是图集增量必须折在 Update 侧的原因之一。

## §4 追问二：字体纹理呢——RAM 图集是缓存，不是每帧重建

图集本体在 egui Context 的 TextureManager 里（CPU 侧 `epaint::TextureAtlas`），**只在有新字形需求时才变**：首次渲染某个字符、或图集满了触发增长重排。变化以 `textures_delta` 报出来——整图增量（重排/增长，`pos: None`）或 dirty-rect 补丁（`pos: Some([x,y])`）。

我们的链路对两种形态的消化方式不同：

- **Update 侧**（`AtlasMirror::fold`）：整图落位、dirty-rect 就 blit 进 CPU 镜像，任何折入 `generation +1`。
- **GPU 侧**（`paint_overlay`）：只看代数差——镜像代数领先已传代数就**整图重传全新 GpuImage + publish 新槽 + 旧代 graveyard**（uploader 图像段只支持整图拷贝，3.7.2 的路线取舍；dirty-rect 与整图对 GPU 侧没有区别）。

**稳态是零上传**：所有用过的字形都进缓存后，`textures_delta` 每帧为空、代数不动、没有批次。收账形状即如此——0.85s 内 13 代字形预热 + 1 次迟发，随后稳态零批。预热需要 13 代是因为调试窗口统计文本（fps 数字、票据号）随时间冒出新字符。

所以每帧真正的重复成本只有三样：**pass 重跑（UI 代码求值）、镶嵌（`tessellate` 产顶点）、顶点环整帧重写 + 逐 clip 归制**。图集是"用到才长、长完就定"的驻留资源，和场景贴图同一种脾气。

## §5 frenderer Dear ImGui 对照

frenderer 与本项目做的是同一道题：**把一个立即模式 GUI 亲手挂到自己的 Vulkan 渲染器上**——连自写 GPU 后端都同（frenderer 没用官方 `ImGui_ImplVulkan`，在自己框架的 RenderTool 线上写了一个，`gui_renderer_render.rs` 全部 257 行）。三件活的分工两边几乎镜像。

### 5.0 总览表

| 维度 | frenderer × Dear ImGui | ash_renderer × egui |
|---|---|---|
| 输入桥 | 现成：vendored `imgui-winit-support` | 自己写：`overlay/input.rs` |
| 输入源 | winit 事件直通，全局序天然保全 | bevy 消息缓冲，跨类无序，按类近似排序 |
| 每帧 pass | `new_frame()` → 闭包 → `render()` | `begin_pass` → 闭包 → `end_pass` |
| 镶嵌落点 | core 内部做完，`render()` 直接给 DrawData（已镶嵌） | `end_pass` 给 shapes，`tessellate` 后端自己做 |
| 顶点上传 | 每帧新建 staging+设备 buffer ×2，等死式 | 固定 HOST_VISIBLE 环，零分配直写 |
| 坐标变换 | CPU 正交矩阵 64B push（y 翻在矩阵） | 16B push（尺寸 + 两 u32）+ shader 公式 + naga 翻转 |
| 纹理选择 | TextureId→descriptor set 查表，逐 draw 重绑 | bindless 槽位 u32 走 push，零重绑 |
| 字体图集 | 启动一次静态全量（全 CJK 预烘焙） | 按需栅格化，dirty-rect→镜像→整传新槽 |
| blend | `SRC_ALPHA/ONE_MINUS_SRC_ALPHA`（非预乘） | `ONE/ONE_MINUS_SRC_ALPHA`（预乘） |
| 渲染形态 | 经典 render pass + framebuffer（RenderTool） | dynamic rendering 同实例接画 |

### 5.1 输入桥：谁拥有窗口，决定这活是"拿来"还是"重写"

frenderer 自己拥有 winit——事件循环就是它的，`handle_event(&event, window)`（`gui_resource/mod.rs`）拿到的是**有序单流**。所以输入桥直接用社区现成的 `imgui-winit-support`（repo 内 vendored），"指针先于按键"这种排序操心在那里根本不存在——winit 单流里按下事件天然带着按下时的最新位置。

本项目这边的全部额外复杂度（按类排序、`pointer_pos` 补位、修饰键快照）都来自同一个事实：**bevy 拥有窗口**，拿到的是被 ECS 消息系统拆成若干独立缓冲之后的事件。不是 egui 比 imgui 难接，是宿主换了——bevy 那层把 winit 单流打散了，桥要重建序。

顺带印证 §4/§5 追问一：Dear ImGui 也是 Context 常驻、闭包每帧重跑、记忆全在 context 里，两边同构；frenderer `set_ini_filename(None)` 关掉了 imgui 默认的磁盘布局持久化，记忆只留进程内。

### 5.2 顶点上传：同一块 20B，两种时间形状（新侦察发现）

顶点格式两边**逐字节相同**：pos/uv/col @ 0/8/16、20B 步长——epaint 的顶点布局就是行业事实标准（imgui 定的形状），frenderer 管线声明（`imgui_render_tool_builder.rs`）与本项目 `UiVertex` 是同一张表。

分歧在数据怎么上 GPU：

- **frenderer**：`bind_meshes`（`gui_renderer_render.rs:141`）每帧调 `ROBuffer::from_data(&vertices)` ×2（顶点/索引各一遍）——每次新建一个 HOST_VISIBLE staging、新建一个 DEVICE_LOCAL buffer、`copied_from` 走"临时命令缓冲 + 新建 fence + CPU 等死"。更狠的是旧 buffer 的下场：赋值换掉 `gui_mesh.vertex_buffer` 时旧的被 Drop，而 `RenderBuffer::drop` 调 **`device_wait_idle()`**（`render_vulkan/src/buffer/render_buffer/mod.rs:46`）——每销毁一个 buffer 等**整个 GPU** 空闲。合计每帧 UI ≥ 4 次全设备等死 + 4 次分配 + 2 次同步提交，CPU 与 GPU 完全串行化，场景渲染也被连坐。**这是 frenderer"wait 等死"纪律在 GUI 顶点通道的复发——此前各轮对照只覆盖上传（staging/图集）链，此路径首度侦察到（2026-10-08）。**
- **本项目**：固定容量 HOST_VISIBLE 环（`MAX_FRAMES_IN_FLIGHT` 对 buffer），每帧偏移 0 直写、零分配零提交；复用安全由 `wait_for_slot` 的 fence 背书——与帧 UBO 同一条纪律。

公道话：frenderer 的做法**功能完全正确**，调试 GUI 顶点量小、单队列场景跑得动——"最短路径打穿"的合理工具选择；它暴露的不是 bug，而是**没有时序契约这层语言**时最自然的写法长什么样。

### 5.3 纹理系统：传统绑定 vs bindless 的教科书对照

判定线"资源选择权在哪"的两个极：

- **frenderer**（传统绑定）：descriptor layout 预留固定少量 combined 采样槽；字体图集启动时**一次性静态全量**——`build_rgba32_texture()` + `glyph_ranges = chinese_full()` 把整个 CJK 范围预烘焙成一张大图（`gui_resource/mod.rs:86`），同步上传一次，之后**永不变化**。运行时换纹理 = `TextureId → descriptor_set` 查表 + 逐 draw `BindDescriptorSets` 重绑（font 用 `usize::MAX` 哨兵约定）。"选哪张"活在绑定调用状态里。
- **本项目**（bindless）：图集是活的（13 代预热），换代的代价只是"槽位账本多一行 + push 一个新 u32"——画 14 代图集，set0 一次没重绑过。图集可随意换代、将来用户贴图随便加，加多少绑定成本都不涨。

两条路线还有一个互相印证：frenderer 的静态图集 + 传统绑定是**自洽的一对**（图集不变，descriptor set 更新的麻烦永远不会发生）；本项目的动态图集 + bindless 也是自洽的一对（图集会变，变化被表吸收）。不自洽的组合才难看——传统绑定 + 频繁换代的图集 = 每帧重分配 descriptor set。

### 5.4 坐标与 blend：两个小差异说透一件事

**y 翻转落点**：frenderer 用 CPU 端正交矩阵（64B Matrix4 push），`vk_orthographic(0, w, 0, -h, ...)` 的 `-2/tmb` 就是翻转本身（`gui_renderer_render.rs:83`）；本项目用 shader 公式 + naga `ADJUST_COORDINATE_SPACE` 注入翻转，push 只 16B。信息量相同、"翻转恰好一次"的 invariant 两边都成立——翻译到 clip 空间这步一个外包给矩阵、一个自己写公式。

**blend 公式严格对不上，且都对**：frenderer 是 `SRC_ALPHA/ONE_MINUS_SRC_ALPHA`（`render_vulkan/src/pipeline.rs:88`，官方 ImGui Vulkan 后端同款），前提是 Dear ImGui 顶点色**非预乘**；本项目是 `ONE/ONE_MINUS_SRC_ALPHA`（照 egui-wgpu 判据），前提是 egui 顶点色**已预乘**。两家的顶点色契约差半格，blend 公式就差半格——各自配平。这正是"blend 判据要照 GUI 库官方后端源码钉死"的原因，公式不能凭感觉通用。

### 5.5 收束

两边"立即模式 UI 接自研 Vulkan 渲染器"的架构完全同构（立即模式 core 常驻、每帧重跑闭包、顶点同是 20B 三件套、逐 draw scissor），**全部实质差异来自两个维度**：数据的时间形状（每帧等死分配 vs 帧槽环 + 票据 + fence）和选择权的住所（绑定调用 vs push 进索引数据）。前者是 3.2/3.7 一路攒下的时序契约，后者是 bindless 判定线本身。frenderer 是每个环节都走最短路径的合法答案；本项目的版本是每个环节都配上显式契约的同一道题。
