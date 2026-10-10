# tools/ 工具清单

仓库根的 `tools/` 分两类：bevy 上游自带的 Rust 工具 crate（随 upstream 同步，本地不改）与本项目 ash_renderer 验证用的 Python 脚本。脚本运行产物一律落 `.temp/`（见 `.agents/rules/临时文件与工具脚本存放.md`）。

## 上游工具 crate

- `ci/`——bevy 仓库 CI 的本地执行器（`cargo run -p ci` 跑检查）。
- `build-templated-pages/`——生成 Bevy 官网的模板化页面。
- `build-wasm-example/`——把示例构建成 wasm/web 版本。
- `build-easefunction-graphs/`——为 `EaseFunction` 文档生成缓动曲线图（SVG）。
- `example-showcase/`——批量跑全部示例或生成官网 showcase 页面。
- `export-content/`——从 `_release-content/` 生成发布内容文件。
- `compile_fail_utils/`——compile-fail 测试辅助库（配合 `tools/ci`）。

## 本项目 Python 工具（ash_renderer 窗口验证）

真实输入注入（含抢焦点）仅在用户声明"无人模式"时执行，见 `.agents/rules/鼠标操作与输入注入纪律.md`；截图、日志、PostMessage 不受限。

- `capture_window.py`——按 PID 抓主窗口截图：PrintWindow（DPI 感知）为主通道；检出 DWM 白图（非白占比 <10%——目标窗口被 TOPMOST 全屏覆盖如 DLP 锁屏海报时，DWM 拒绝被完全遮挡窗口的内容重定向）时自动把目标窗口 TOPMOST 提顶重试后复原 Z 序；stdout 标注实际通道并报 hwnd/rect/尺寸/非白占比/md5。注意：锁屏会话（LogonUI 在跑）期间 PrintWindow 恒白、注入输入不可达（安全桌面边界），视觉验证须等解锁。
- `inject_mouse.py`——SendInput 注入鼠标输入（DPI 感知）：`drag` 左键拖拽、`move` 纯移动不按键、`wheel` 滚轮，坐标为屏幕物理像素。
- `focus_window.py`——把目标窗口置前台（AttachThreadInput 套路），注入输入前必须先执行。

## 本项目 Python 工具（ash_renderer BRP 直控）

- `brp.py`——BRP 薄客户端（3.12/3.13）：把宿主 HTTP JSON-RPC（127.0.0.1:15702）封装成十一个子命令——`tree`/`inspect`/`query`/`set-transform`/`camera`/`mouse-move`/`mouse-button`/`mouse-wheel`/`mouse-status`/`discover`/`raw`；组件短名自动展开，AI 免记协议。虚拟鼠标走应用层消息缓冲（与 bevy_winit 同层），不碰真实光标/焦点，不受无人模式纪律约束。逐命令用法与实战配方见 [BRP直控使用指南](../.agents/docs/3-静态取数链路/3.12-CLI直控/BRP直控使用指南：逐命令用法、与MCP的分界及Skill+CLI定位.md)。
