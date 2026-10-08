# UI点击偏移订正讲解：tessellate顶点域、push尺寸域与DPI测量假阳性

> 定位：3.7 egui 调试 GUI 收官后一笔用户实测 bug（3.7.5）的讲解篇——记录"点击要在按钮下方才点得到"的完整因果链：域失配的机制、三版本演进、以及为什么当时的烟雾验证没抓到。施工账与判定线证据在[3.7.4 回归收官 §7](3.7.4-回归收官：判定线全量、ppp双域订正与输入排查链.md)；修复代码 = feat d90f14369。配合阅读：[egui裸接ash讲解](egui裸接ash讲解：输入桥、pass与GPU后端（兼frenderer-Dear-ImGui对照）.md)（§2 屏幕域与镶嵌落点）。

## §0 判定线

1. **egui `ctx.tessellate(shapes, ppp)` 输出的顶点在 points 域——ppp 从不缩放顶点位置**。ppp 在镶嵌里只进三处：字形栅格化密度（galley 的 ppp 与当前不一致才重栅格）、像素栅格取整（round_to_pixels）、圆盘半径缩放。顶点着色器 `pos * 2.0 / push.screen_size - 1.0` 里的 screen_size 必须与顶点同域，即 **points = 物理尺寸 ÷ ppp**（egui-wgpu 0.36.2 判据：uniform 字段名就叫 `screen_size_in_points`，取值 `size_in_pixels / ppp`）。
2. **症状句式**：UI 整体等比 ×ppp 缩小（1.25 时 ×0.8）且锚在左上、点击必须落在画出来的按钮右下方 ×ppp 处才命中 → push 尺寸域与顶点域失配（push 给了物理像素）。反过来 push 若给 points÷ppp 才会出现 ×ppp² 放大——两个错法方向相反，别混。
3. **测量/注入脚本的进程必须先 `SetProcessDpiAwareness(2)`**：DPI 不感知进程的 GetClientRect / SetCursorPos / PrintWindow 全被系统虚拟化（÷缩放系数）。测量与注入两把尺子同缩时**错误互相抵消**，注入点击"看似命中"而真实用户全偏——这是本次 bug 溜过烟雾验证的机制。

## §1 症状与两条线索

用户实测：调试面板的单选钮，鼠标要放在**画出来的按钮下方（偏右下）**才点得到；并且明确说"之前某个版本是能正确点击的"。

两条线索各自指向一半真相：

- "要偏右下才点得到" → 绘制位置与命中区有固定比例的错位，方向 = 命中区在绘制区的右下 ×1.25（DPI 125%）。
- "之前某版本能点" → 那个版本的 push 值是对的；后来某次"修复"把它改错了。git 钉死：3.7.3 首版 push = points（对），3.7.3 收官提交 1a233a3ad 把它改成物理像素（错），即用户体感里的两个版本。

## §2 三版本演进账

| 版本 | push 值（ppp=1.25，屏 1600×900 物理 / 1280×720pt） | UI 渲染位置 | 命中区 | 结果 |
|---|---|---|---|---|
| 3.7.3 首版（`UiPaint::screen_points` = 1280×720） | points | pt × 1.25 ✓ | pt × 1.25 | **正确**——用户记忆里"能正确点击的版本" |
| 3.7.3 收官（`UiPaint::screen_px` = 1600×900） | 物理像素 | pt × 1.0（= 正确位置 × 0.8，锚左上） | pt × 1.25 | **用户实测 bug**：须点画面右下 ×1.25 处 |
| 3.7.5 订正（`UiPaint::screen_pt` = extent ÷ ppp = 1280×720） | points（基准换实测 extent，更诚实） | pt × 1.25 ✓ | pt × 1.25 | 正确，回归全绿 |

注意首版与订正版 push 数值相同——差异只在取值来源：首版取 egui 的 screen_rect（窗口逻辑尺寸账面值），订正版取 swapchain 实测 extent ÷ frame.pixels_per_point。账面与实测一致时两者相等；取 extent 的意义在 resize/账面失真时仍与真实渲染目标对账。

3.7.3 收官提交信息里"首版 ×ppp² 双重放大、点击全部错位"的叙事**不成立**（提交历史不重写，以本文与 3.7.4 §7 为准）：首版行为是上表第一行，本来就对。当时的"×1.56"读数出自 DPI 不感知的测量链（§4），把正确实现"订正"成了错的。

## §3 机制：三个域，各喂各的

UI 点击链上其实有三个尺寸域在同时工作，谁都不许替谁换算：

1. **egui 布局/命中域（points）**：`RawInput.screen_rect`、面板 rect、命中测试全在 points。bevy_winit 把 `physical_position / scale_factor()` 喂给输入桥（crates/bevy_winit/src/state.rs），所以真实鼠标进 egui 的坐标恒 = 物理像素 ÷ ppp——这半边从头到尾是对的，本次全程无辜。
2. **镶嵌输出（points）**：`tessellate` 产出的顶点位置就是布局域原值。ppp 的三处真实用途（epaint 0.36.2 tessellator.rs）：字形 galley 密度（`pixels_per_point` 不匹配才按新密度重栅格，约 :2012）、像素栅格取整（round_to_pixels）、圆盘半径缩放（约 :1508）。**没有任何一处把顶点位置乘 ppp**。
3. **渲染目标域（物理像素）**：swapchain extent、viewport、scissor。"points 换算成像素"是渲染目标侧的职责——具体落在两处：push 的 screen_size（shader 公式的分母，必须用 points）与 scissor（clip rect × ppp，必须用物理）。

于是域失配只有两种错法、方向相反：

- push 给物理（= extent）：NDC 分母偏大 ppp 倍 → 渲染位置 = pt × extent/S = pt × 1.0，即正确位置 × 0.8。命中区不动（仍在 pt×1.25）→ 用户要点在绘制区的右下 ×1.25。**本 bug**。
- push 给 points÷ppp（= 1024×576）：渲染位置 = pt × 1.5625 = ×ppp² 放大并越界。这是 3.7.3 误诊时以为首版得了的病——实际没有任何版本处于这一行。

修复后的对账恒等式：`screen_pt = extent ÷ ppp`，顶点（points）÷ screen_pt = NDC，恰好把 points 域铺满物理屏幕一次；scissor = clip × ppp 物理域，与渲染位置同基准。

## §4 假阳性机制：两把虚拟尺子互相抵消

3.7.4 的烟雾验证（注入点击 + 截图对账）全绿、真实用户全偏，原因是一条测量通道级的错误：

- DPI 不感知进程里，**SetCursorPos 的入参与 GetCursorPos 的回读都被系统按同一比例虚拟化**（本机 125% → ÷1.25）。注入"目标 (x,y)"时系统把坐标当虚拟坐标换算到真实位置，回读又从真实位置换算回虚拟值——注入侧自洽，但**落进窗口的真实物理位置与意图差一个 ×1.25**。
- 同一进程里的 PrintWindow 截图同样被虚拟化（3.7.4 已钉过：PrintWindow 885×582 ↔ swapchain 1107×728）。尺寸对账时"图上读数"与"swapchain 读数"差同一个比例，解读时若不注明来源就会推出假的缩放系数。
- 两把尺子**同向同比例错** → 注入点击"命中"截图"对上"，错误在验证闭环里互相抵消；唯一没有虚拟化的是真实用户的鼠标——于是 bug 只在真人手上现形。

这条钉子已写入[画面功能视觉验证规则](../../../rules/画面功能视觉验证.md) §1：测量/注入脚本开头先 `SetProcessDpiAwareness(2)`，记录尺寸时注明读数来源（PrintWindow 尺寸 vs swapchain 尺寸）。修复后的复测全部跑在 DPI 感知进程里：探针色块 [700..800×300..350]pt 在 1600×900 真实截图里精确落在 (875,375)、尺寸 125×62.5（=pt×1.25）；注入点击画出来的 unlit 按钮中心 → 模式切换成功；用户真实鼠标同样命中。

## §5 对账方法论（下次直接抄）

1. **探针色块法**：在 egui 层画已知 pt 矩形的实心色块（放在面板外），PrintWindow 抓图后按颜色求 bbox——直接量出"pt→真实像素"的实际缩放系数，一步判定绘制域。比读文字/控件位置可靠一个量级。
2. **点击探针三域对账**：点击时打一行日志——egui 收到的 pointer（points）、面板 rect（points）、本帧 screen_rect（points）。pointer = 注入物理坐标 ÷ ppp 成立 ⇒ 输入桥清白；面板 rect 与实测渲染 bbox 对不上 ⇒ 渲染域错。先分锅再修。
3. **权威判据 = 源码，不是推断**：egui 顶点域这类"上游库把谁换算成谁"的问题，判据按此优先级：官方渲染器同位的字段名/取值（egui-wgpu `UniformBuffer::screen_size_in_points`）→ 库源码通读（epaint tessellator 的 ppp 用途）→ 自建探针实测。推断（"tessellate 应该会换算"）不足以定案——本次误诊正是推断压过了源码。

## §6 修复与回归收账

- 代码：`UiPaint::screen_px` → `screen_pt`（`vulkan/overlay_pipeline.rs` 契约 + `overlay/paint.rs` 组装 extent÷ppp + `frames.rs` pack 点 + `overlay_draw.wgsl` 注释），feat d90f14369；scissor 保持 clip×ppp 物理域（该半边本来就对）。
- 回归全绿：clippy 全净、零 VUID（含拆 Device）、3.5 判定线零回退、resize×3 域对账跟随新 extent、最小化/还原闸门、WM_CLOSE exit 0；视觉验证（当前模型读图，探针色块清零后）通过。
- 证据截图：[egui-ppp-fix-probe-green.png](../_assets/egui-ppp-fix-probe-green.png)（探针 build：色块落 (875,375) 尺寸 125×62.5）、[egui-ppp-fix-final.png](../_assets/egui-ppp-fix-final.png)（干净 build 最终界面，面板尺寸/位置正常）。
- 边界：修复时加装的四类临时探针（色块/点击三域/域对账/模式变化打点）已全部撤除，证据数值以本文与 3.7.4 §7 表格为准。
