---
name: publish
description: 编译 ash_renderer 并把可执行文件拷贝到仓库根 publish/ 目录，产出一个冻结的正式版，让用户在 AI 继续编译、自动测试的同时游玩；正式版 BRP 自动换到 15710 端口，不与开发实例冲突。当用户输入 "/publish"，或要求"发布稳定版/正式版""拷贝编译产物到 publish""出一个能玩的版本"时使用。
---

# /publish：发布可并行游玩的正式版

把当前代码编成独立 exe 放到 `publish/`，与 `target/` 下的开发构建彻底分开：

- Windows 会锁住运行中的 exe。开发构建一旦在跑，cargo 就无法覆盖它；拷贝到 `publish/` 后，开发侧照常编译。
- shader 用 `include_bytes!` 编进 exe，资产根是编译期写死的仓库 `assets/` 绝对路径，所以单个 exe 即可独立运行。
- 正式版与开发实例是同一份程序。宿主启动时比较 exe 的实际位置和编译期记下的 `target/<profile>/` 目录（`ash_renderer/src/remote/mod.rs` 的 `is_dev_instance`）：在里面就是开发实例，用 BRP 默认端口 15702；拷出去就是正式版，用 15710。自动测试按 15702 连接，不会打到正在玩的正式版。

## 步骤

1. `git status` 报告当前提交和未提交改动；发布内容 = 当前工作区代码（含未提交改动），在报告里如实写明。
2. 运行脚本（默认 release，目标 `<仓库根>/publish/`）：

   ```powershell
   powershell -ExecutionPolicy Bypass -File .agents/skills/publish/scripts/publish.ps1
   ```

   可选参数：`-BuildProfile dev|release|<自定义 profile>`、`-Dest <目录>`（必须在 target 目录之外）、`-SkipBuild`（只拷贝已有产物）。
3. `publish/` 里的正式版正在运行时脚本会拒绝覆盖。停下告诉用户先关闭，不要擅自结束进程。
4. 报告发布路径、`VERSION.txt` 内容（提交号、profile、BRP 地址、时间）和启动方式：直接运行 `publish\ash_renderer.exe`。启动日志的 BRP 行会标明"正式版"和实际端口。

## 端口规则

| 启动方式 | BRP 主端口（render subapp +1） |
|---|---|
| 从 `target/<profile>/` 启动（`cargo run`、直接跑构建产物） | 15702 |
| 拷到 target 之外启动（publish 正式版） | 15710 |
| 任意方式 + 环境变量 `ASH_BRP_PORT` | 环境变量的值 |

用 BRP 查询或驱动正式版时连 15710。同时开两个正式版会撞端口，此时给其中一个设 `ASH_BRP_PORT`。

## 注意

- 正式版读的仍是仓库 `assets/`，资产改动会影响它；代码和 shader 已冻结在 exe 里。
- `publish/` 已加入 `.gitignore`，不提交。
