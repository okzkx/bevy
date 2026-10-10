# 3.12：CLI 直控（AI 通道，BRP）

对应[执行路线图](../../学习目标实现步骤.md)步骤 3 节尾追加段立案。本目录是 3.12 段的任务面板与证据落位。

**目的**：补齐"免 UI 改内容"——宿主 World 经 BRP（HTTP JSON-RPC，127.0.0.1:15702）暴露：AI/脚本不开 UI 即可"查询 → 改 Transform → 相机环绕推拉"，为步骤 4 的对象/资产增量与驻留账本回归备好操纵通道。

**状态：✅ 已收官（2026-10-10）——连接闭环全过（五张证据图读图验证通过 + 负例三连 + 零 VUID + WM_CLOSE exit 0）；属性面板数值联动（选中态）的用户人工轮已完成（2026-10-10 用户确认）。**施工记录：[3.12-CLI直控施工记录：BRP方法族、相机CLI输入源与连接闭环](3.12-CLI直控施工记录：BRP方法族、相机CLI输入源与连接闭环.md)。

## 技术底座（开工先读，勿重复侦察）

- [BRP 侦察笔记](../../笔记/bevy_remote：BRP远程协议与自定义方法.md)：协议两层（RemotePlugin 协议核心 + RemoteHttpPlugin HTTP 传输）、内置方法全景、自定义方法=system、安全边界（loopback 无鉴权）。0.20 改名警告：`world.*` 方法族，旧资料 `bevy.query` 风格会 METHOD_NOT_FOUND。
- 3.11 底座：`apply_transform_edit` 唯一实现（写侧复用点，立案原话"3.12 的 BRP 方法调同款实现，不养两份逻辑"）。
- 3.9 底座：`CameraOrbit` 球坐标组件（机制半边驱动）——CLI 给值做成同模块封装 `apply_camera_command`，与鼠标共用限位与 Transform 写路径。

## 任务面板

- [x] **启用 bevy_remote + 官方读方法族**：bevy 加开 `bevy_remote` feature（Cargo.toml 注释记偏离合账）；`world.inspect`/`world.query`/`world.summarize` 等随 RemotePlugin 默认注册，零自研。

  判定线：官方方法族在禁渲染宿主实跑可用；短名组件可查（薄客户端自动展开）。

  **施工结果**：全通——inspect/query 短名查询实测返回反射值；`discover` 列 36 方法（含三个自定义）。

- [x] **自定义方法三个**（`src/remote.rs`，AshRemotePlugin）：
  1. `ash_renderer/scene_tree`——层级树，行数据/相关性过滤/节点命名复用 3.10 egui 树窗口（`EntityRow`/`is_relevant`/`node_label` 升 `pub(crate)` 重出口），CLI 树与面板树同一棵树；
  2. `ash_renderer/set_transform`——独占 `&mut World` 直调 `apply_transform_edit`（3.11 唯一实现复用），至少一组量，成功返回写后读回值；
  3. `ash_renderer/set_camera`——相机给值，机制半边新增 `apply_camera_command`（target/yaw_deg/pitch_deg/radius/zoom 逐项 Option；限位与写路径与鼠标共用；3.9 的 CLI 输入源）。

  判定线：写侧不养两份逻辑（两封装各只有一处实现）；CLI 给值与鼠标并存胜负时序钉明（同帧后写生效、共享状态零跳变，记录 §4.5）。

  **施工结果**：三方法全部实测（读回 + 像素双证）；胜负时序钉在记录 §4.5。

- [x] **`tools/brp.py` 薄客户端**：tree/inspect/query/set-transform/camera/discover/raw 七子命令，组件短名自动展开（world.list_components 尾段匹配），标准库零三方依赖；BRP 错误原样打印 exit 1、连不上 exit 2。

  判定线（立案独立验收）：AI 不开 UI 完成"查询→改 Transform→相机环绕推拉"闭环并留 PrintWindow 证据；CLI 与 egui 并存；无鉴权仅本地；零 VUID、WM_CLOSE exit 0。

  **施工结果**：闭环五步全过（基线→transform→yaw 环绕→zoom 推拉→复原，证据图五张，读图验证全部通过）；负例三连（-23401 原码回传/-32602×2）宿主存活；零 VUID 实账（grep 计数 0）；WM_CLOSE exit 0；clippy 全净。**属性面板数值联动：用户人工轮已完成（2026-10-10 用户确认）**（施工当时选中态需点击注入、本会话未声明无人模式；写链路有读回+像素双证）。

## 边界

- 实例化（spawn）/despawn 的 BRP 方法顺延步骤 4（4.1 对象增量落地后补）。
- MCP 是另一层传输适配（把 BRP 封装成 MCP server 给通用 MCP 客户端），另立专题不并入；本段连接通道 = BRP + `tools/brp.py`。
- web console / CORS 形态不并入（立案原样）。
- 无鉴权仅本地 loopback，不暴露公网（侦察笔记边界，未动）。

## 材料

- [BRP直控使用指南：逐命令用法、与MCP的分界及Skill+CLI定位](BRP直控使用指南：逐命令用法、与MCP的分界及Skill+CLI定位.md)——**使用手册 + 概念定位篇**：七子命令逐条用法与退出码口径、BRP 与 MCP 的叠层分界（服务端是谁/给谁用/暴露什么，MCP 可包 BRP 做薄适配另立专题）、"Skill+CLI"三层形状定案（知识在文档/执行在脚本/通道在协议）与实战配方。
- [3.12-CLI直控施工记录：BRP方法族、相机CLI输入源与连接闭环](3.12-CLI直控施工记录：BRP方法族、相机CLI输入源与连接闭环.md)——施工记录（2026-10-10）：结构定案（读侧官方复用+scene_tree 树形复用 3.10、写侧两封装直调）、连接闭环实跑五步证据、负例三连、钉子六枚（handler 错误路径机制口径、world.query 单泛型、discover 对象表、后台起宿主最小化前置、CLI/鼠标胜负时序、相机初值回读链）。
- PrintWindow 证据图五张（基线/transform/环绕/推拉/复原）——临时取证产物，按《临时文件与工具脚本存放》不入知识库；读图结论留档施工记录 §3.2。
