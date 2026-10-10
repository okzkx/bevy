# 3.13：Remote 虚拟鼠标（不基于窗口的输入源）

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 节尾追加段立案。本目录是 3.13 段的任务面板与证据落位。

**目的**：给 Remote 操作者（AI/脚本）一套窗口无关的鼠标——移动/按下/抬起/滚轮经 BRP 注入引擎输入管线，与真实鼠标在同一缓冲合流；AI 不碰真实光标即可驱动相机轨道、操作 egui 面板。顺带把 3.12 遗留的"属性面板数值联动"人工轮补成 AI 自验闭环（远程点选树节点 → CLI 改值 → 面板同步）。

**状态：✅ 已收官（2026-10-10）——判定线全过：不碰真实鼠标完成"移动→按下→拖拽→抬起→滚轮"数字闭环（orbit 读回逐位吻合：拖拽 +80.000°/−45.000°、滚轮 ×0.95³ 精确）+ 六张证据图读图验证通过 + 虚拟点选 egui 树行/属性面板联动成立 + 负例三连宿主存活 + 零 VUID + WM_CLOSE exit 0 + clippy 全净；3.12 遗留"属性面板数值联动"AI 自验轮随段收账。**施工记录：[3.13-Remote虚拟鼠标施工记录：WindowEvent注入层、四方法与AI自验闭环](3.13-Remote虚拟鼠标施工记录：WindowEvent注入层、四方法与AI自验闭环.md)。

## 技术底座（开工先读，勿重复侦察）

- **注入层 = `Messages<WindowEvent>`**（与 bevy_winit 喂引擎的同一层）：官方拆分系统 `send_typed_window_events`（bevy_window/src/system.rs:20，PreUpdate `WindowEventSystems`，`.before(InputSystems)`）把事件拆成类型化消息，其中 `CursorMoved` 分支同步更新 `Window.internal.physical_cursor_position`（system.rs:45-62）——光标位走官方通路自管，`window.cursor_position()` 直接可见。
- **刻意不走 `Window::set_cursor_position`**：它在写入光标位的同时还写 `cursor_position_request`，bevy_winit 会拿去真实挪动 OS 光标（违反[输入注入纪律](../../../rules/鼠标操作与输入注入纪律.md)）。虚拟鼠标只进引擎事件缓冲，不经 OS 输入队列。
- **下游零改动**：`ButtonInput<MouseButton>`/`AccumulatedMouseScroll`（bevy_input PreUpdate）、egui 输入桥（overlay/input.rs 读 `CursorMoved`/`MouseButtonInput`/`MouseWheel`）、相机轨道控制（scene/mechanism/camera_control.rs 读按键 + `cursor_position()`）全部现成。
- **时序**：handler 在 RemoteLast 写消息 → 次帧 First 换缓冲 → PreUpdate 拆分+输入系统更新 → Update 消费。写值下一帧生效，与 `set_transform`/`set_camera` 同口径。
- **3.12 底座**：`AshRemotePlugin`（方法注册单点）+ `tools/brp.py` 薄客户端（本段加 mouse 子命令族）。

## 任务面板

- [x] **虚拟鼠标四方法**（新模块 `src/remote_mouse.rs`，注册收在 `AshRemotePlugin`）：
  1. `ash_renderer/mouse_move`——`{x, y}`（points 域逻辑像素，左上原点）→ `WindowEvent::CursorMoved`；
  2. `ash_renderer/mouse_button`——`{button: left|right|middle|back|forward, action: press|release}`，另备 `{"action": "release", "all": true}` 释放全部按住键（AI 按下后连接断掉的保险）→ `WindowEvent::MouseButtonInput`；
  3. `ash_renderer/mouse_wheel`——`{lines|pixels}` 二选一（垂直滚动）→ `WindowEvent::MouseWheel`；
  4. `ash_renderer/mouse_status`——位置/按住键/窗口尺寸/本帧滚轮读回（操作者反馈环）。

  判定线：注入点=WindowEvent 层（协议与 bevy_winit 同层，下游零特判）；坐标口径与 `Window::cursor_position`/egui 同域；非有限值、未知按钮、滚轮双填/双缺拒绝（-32602）；一帧延迟口径与既有方法一致。

  **施工结果**：全过——`discover` 列 40 方法（36+4）；注入层三层实比定案 `Messages<WindowEvent>`（`set_cursor_position` 挪真光标出、类型化直注 Window 光标位不更新出，见施工记录 §4.1）；负例 -32602 ×3 实抓。

- [x] **`tools/brp.py` mouse 子命令族**：`mouse-move`/`mouse-button`/`mouse-wheel`/`mouse-status` 四子命令（合计十一子命令）。

  判定线：AI 免记协议完成"移动→按下→拖拽→抬起→滚轮"逐命令闭环；错误退出码口径与既有七子命令一致（BRP 错误 exit 1、连不上 exit 2）。

  **施工结果**：闭环逐命令实跑（§3.1 数字表）；负例 exit 1；文档头补虚拟鼠标口径（先 move 再 press、不受无人模式纪律约束）。

- [x] **连接闭环验证**：不起 UI 交互（不碰真实鼠标/焦点）完成轨道拖拽 + 滚轮推拉 + egui 面板远程点选，PrintWindow 证据图（当前会话模型读图验证）+ 读回值双证；负例三连宿主存活；零 VUID；WM_CLOSE exit 0；clippy 全净。

  （顺带收账：3.12 遗留"属性面板数值联动"的 AI 自验轮——远程点选选中态后 CLI 改值，面板同步。）

  **施工结果**：全过——数字闭环（§3.1）+ 六张证据图读图验证（§3.2：基线/拖拽/滚轮/点选/面板联动/复原）+ 负例三连 + 零 VUID（grep 计数 0）+ WM_CLOSE exit 0；**3.12 联动收账=证据图 05**（虚拟点选 Hose_low → CLI 改 translation y=0.2 → 面板同步显示）。

## 边界

- 键盘/文本/手势合成不并入（鼠标先立；键盘将来按需另补，不预立案）。
- 不做独占模式：真实鼠标与虚拟鼠标同一缓冲合流、后到者赢——语义是"第二只鼠标"，非替换。独占/屏蔽真实输入等需求出现再立案。
- 固定主窗（PrimaryWindow），多窗寻址不做。
- `tools/inject_mouse.py`（SendInput 真输入，无人模式纪律管束）与本段通道分界在施工记录钉明：本段注入发生在应用层消息缓冲，不碰用户真实光标/焦点/剪贴板，不受无人模式纪律约束。
- 无鉴权仅本地 loopback（3.12 侦察笔记边界原样，未动）。

## 材料

- [3.13-Remote虚拟鼠标施工记录：WindowEvent注入层、四方法与AI自验闭环](3.13-Remote虚拟鼠标施工记录：WindowEvent注入层、四方法与AI自验闭环.md)——施工记录（2026-10-10）：注入层三层对照定案、数字/像素双闭环（orbit 读回逐位吻合 + 六张证据图）、3.12 联动 AI 自验轮收账、钉子八枚（注入层对照/时序账/合流语义/inject_mouse 分界/按压前提/复原差异/全释结账时点/WM_CLOSE 句柄）。
- PrintWindow 证据图六张（基线/拖拽/滚轮/点选/面板联动/复原）——临时取证产物，按《临时文件与工具脚本存放》不入知识库；读图结论留档施工记录 §3.2。
