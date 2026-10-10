# BRP 直控使用指南：逐命令用法、与 MCP 的分界及 Skill+CLI 定位

> 定位：3.12 交付物的**使用手册 + 概念定位篇**。协议机制细节见[侦察笔记](../../笔记/bevy_remote：BRP远程协议与自定义方法.md)（不重复），施工与实测钉子见[施工记录](3.12-CLI直控施工记录：BRP方法族、相机CLI输入源与连接闭环.md)；本篇回答三个问题——怎么用、和 MCP 差在哪、它是不是"Skill + CLI"。

## 1. 一句话

宿主进程把 World 经 **BRP（HTTP JSON-RPC 2.0）**暴露在 `127.0.0.1:15702`；`tools/brp.py` 把协议封成逐命令形状——AI/脚本**不开 UI** 即可"查层级 → 改 Transform → 相机环绕推拉"，返回值带读回（写后读对账），错误带 BRP 原码。

## 2. 怎么用

### 2.1 起宿主（BRP 随宿主自起，无需任何额外开关）

```bash
cargo run -p ash_renderer          # 或直跑 target/debug/ash_renderer.exe
```

- BRP 服务端随宿主 `Startup` 自起，监听 **127.0.0.1:15702**（`bevy_remote::http::DEFAULT_PORT`）；本宿主禁渲染无 RenderApp，渲染子世界那台 15703 不会起。
- **后台（无前台权）直跑 exe 时主窗会落在最小化态**——要抓截图证据，先 `PostMessage WM_SYSCOMMAND SC_RESTORE` 还原窗口再跑 `tools/capture_window.py`（施工记录 §4.4 钉）。
- 安全边界：默认只绑 loopback、**完全无鉴权**——仅限本机调试，不要改绑对外地址（侦察笔记 §6）。

### 2.2 十一个子命令（全局参数 `--host/--port` 放在子命令前）

```bash
# ① 场景层级树：相关实体过滤 + 树形递归，行文本与 egui 层级树窗口一致
python tools/brp.py tree
#   brp: 相关 16 / 全部 434
#   WorldAssetRoot #398  [398v0]
#     Scene0 #419 ...
#       Hose_low.HoseMat #426  [mesh]  [mat]  [426v0]   ← 尾注实体号可直接喂写命令

# ② 实体详情（官方 world.inspect）：逐组件带反射值与序列化值
python tools/brp.py inspect --entity 426v0

# ③ 实体查询（官方 world.query）：短名自动展开为全限定路径
python tools/brp.py query --data Transform --with Mesh3d     # 取值 + 必须携带
python tools/brp.py query --option all --with Mesh3d         # 一次抓全部可读组件

# ④ 改 Transform（逐组可选；欧拉角 YXZ 度数，与属性面板同口径；成功返回写后读回值）
python tools/brp.py set-transform 426v0 --translation 0 0.5 0
python tools/brp.py set-transform 426v0 --rotation-deg 0 15 0 --scale 1 1.5 1

# ⑤ 相机给值（逐项可选；zoom 与滚轮同语义，<1 推近；限位 pitch ±89°、radius 0.5~10）
python tools/brp.py camera --yaw-deg 75 --zoom 0.7
python tools/brp.py camera --target 0 0.3 0 --pitch-deg 18 --radius 1.28

# ⑥ 虚拟鼠标移动（3.13）：points 域逻辑像素、左上原点（与 egui 同域）
python tools/brp.py mouse-move --x 952.8 --y 369.6

# ⑦ 虚拟鼠标按下/抬起：按压需要指针落点，操作序列先 ⑥ 再 press
python tools/brp.py mouse-button --button left --action press
python tools/brp.py mouse-button --button left --action release
python tools/brp.py mouse-button --action release --all      # 释放全部按住键（卡"按住"保险）

# ⑧ 虚拟滚轮（--lines|--pixels 二选一；正值向上滚 = 相机推近）
python tools/brp.py mouse-wheel --lines 3

# ⑨ 鼠标状态读回：位置/按住键/窗口尺寸与 scale_factor/本帧累计滚轮
python tools/brp.py mouse-status

# ⑩ 方法发现：全量方法名 + 参数形参清单（协议速查，免查文档）
python tools/brp.py discover

# ⑪ 协议兜底：任意 JSON-RPC 原样透传（官方方法族全可用）
python tools/brp.py raw --method world.summarize
python tools/brp.py raw --method world.list_components
```

**行为口径**：实体号格式 `索引v代数`（如 `426v0`，树命令尾注直接可复制）；写值在帧尾（RemoteLast）落账、**下一帧生效**（BRP 以帧为单位量化延迟，稳态约一帧 16ms）；短名解析=向宿主 `world.list_components` 要全量后按"最后一个 `::` 段"唯一匹配，含 `::` 的输入按全路径原样透传。
**虚拟鼠标口径（3.13）**：注入发生在应用层消息缓冲（`Messages<WindowEvent>`，与 bevy_winit 同层），不碰真实光标/焦点/剪贴板，**不受无人模式纪律约束**（`inject_mouse.py` 的 SendInput 才受）；与真实鼠标同一缓冲合流、后到者赢。拖拽配方=move 落点 → press → 逐点 move → release（相邻调用隔 ≥1 帧更贴近真实节奏）；相机拖拽换算 `360°/窗口高`（720pt 时 0.5°/pt）、滚轮每格 ×0.95。
**退出码**：`0` 成功；`1` BRP 层错误（JSON 里 `error.code` 带原码，如 `-23401 ENTITY_NOT_FOUND`、`-32602 INVALID_PARAMS`，宿主不因此受影响）；`2` 连不上（宿主没起/端口不对）。argparse 用法错误也走 2。
**配套证据链**：改值前后各跑 `python tools/capture_window.py --pid <pid> --out .temp/xx.png`（PrintWindow 按句柄），配合当前模型读图对照——3.12 验收的五张证据图即此形状（图片为临时取证产物不入知识库，段 README 留档读图结论）。

### 2.3 宿主侧扩展方法（改协议面时）

方法注册收在 `ash_renderer/src/remote.rs` 的 `AshRemotePlugin::build`（`with_method_main` 链；3.13 起实现按域分模块——场景语义方法住 `remote.rs`，虚拟鼠标住 `remote_mouse.rs`，注册单点不变）；handler 就是一个 Bevy system（入参 `In<Option<Value>>`、返回 `BrpResult`），写侧惯例=独占 `&mut World` 直调既有封装函数（`apply_transform_edit`/`apply_camera_command` 形状），不排队不走 UI、不养两份逻辑。新方法注册后 `discover` 自动列出，无需客户端改代码。

## 3. 与 MCP 的区别

先说共同点：两者都是 **JSON-RPC 2.0** 基底、都有"方法发现 + 调用"的形状——这是容易混淆的根源。分界在**服务端是谁、给谁用、暴露什么**：

| 维度 | BRP（bevy_remote） | MCP（Model Context Protocol） |
|---|---|---|
| 服务端 | **游戏引擎进程内**（插件自起 HTTP server），World 就是后端 | **独立工具进程**，由工具实现方单独编写部署 |
| 客户端 | 任意 HTTP 客户端（curl/Python/浏览器/脚本），零协议依赖 | 必须是支持 MCP 的 AI 宿主（ZCode/Claude Desktop/IDE 集成等） |
| 暴露对象 | 引擎 World 的实体/组件/资源 CRUD + 检视 + 自定义 system 方法——**操作语义固定在 ECS** | 任意"工具"（函数）、资源、提示模板——**语义由工具方自定义** |
| 发现机制 | `rpc.discover` 列方法名+参数形状 | `tools/list` 等原语，配套权限审批/会话管理 |
| 传输 | HTTP（本宿主 loopback:15702） | stdio / HTTP（Streamable）等，面向跨进程跨机 |
| 解决的问题 | **"这个运行中的引擎怎么被外部读写"** | **"AI 宿主怎么发现并安全调用外部工具"** |

对本项目的结论：

1. **3.12 选 BRP 因为服务端免费**——宿主加两行插件就得到 ECS 服务端，官方检视方法族+反射序列化全部现成；MCP 则要求自建 server 进程并自己包一层 ECS 操作，等价能力要多写一整层。
2. **两者不是竞争关系，是叠层关系**：MCP server 完全可以**包着 BRP** 做薄适配（一个 stdio 小进程把 `tree/set-transform/camera` 转成 MCP tools，背后打 HTTP 到 15702）。当前没有 MCP 客户端需求，这层适配**另立专题不并入**；需要时形状已定，半天工作量。
3. 记忆锚：**BRP 把引擎变成"ECS 服务端"；MCP 把工具变成"AI 可发现的服务"**。前者管引擎，后者管 AI 与工具的接线。

## 4. 是否更像 Skill + CLI——是，而且这正是刻意的形状

按本仓库的能力分层（用户级 `AGENTS.md`：Skill=可执行流程与显式入口、知识放 docs），3.12 的实际交付是三层拆分：

| 层 | 3.12 里的对应物 | 职责 |
|---|---|---|
| 通道（协议） | BRP，宿主内嵌 HTTP server | 机器接口：JSON-RPC 方法族，稳定 ABI |
| 执行（CLI） | `tools/brp.py` 十一子命令（3.12 七 + 3.13 mouse 四） | 把协议封成逐命令形状，人/AI 都免记协议细节 |
| 知识（文档） | 本指南 + 侦察笔记 + 施工记录 + 段 README | 何时用、坑点、判定线、扩展惯例 |

这正是 **Skill + CLI** 的经典形状：知识在文档（可读可审）、执行在脚本（可测可组合）、通道在协议（稳定不动）。与 MCP 的取舍：

- **Skill+CLI 赢在轻与透明**：零协议依赖、逐命令可 grep 可组合、知识随仓库走、调试就是看 stdout；代价是**发现靠文档**——AI 得先读到"有 brp.py 这回事"（本项目靠段 README、路线图与记忆指针解决）。
- **MCP 赢在发现与治理**：工具自动列给任何 MCP 客户端、带权限审批与会话管理；代价是多一个常驻进程、多一层封装、调试隔着协议。
- **本项目的判断**：使用面窄（7 个命令）、消费者明确（本仓库的 AI 会话），Skill+CLI 形状够了；**没有注册成 Skill** 是因为"要不要用/怎么用"一页文档可尽，触发场景不复杂——若将来命令族长大或需要跨会话自动匹配触发，再注册一个薄 Skill 入口（正文委托本指南，不复制内容），分层不变。

## 5. 实战配方（照抄即用）

```bash
# 起宿主（后台）→ 等端口 → 拿 PID
./target/debug/ash_renderer.exe > .temp/host.log 2>&1 &
python -c "
import socket, time
for i in range(40):
    s = socket.socket(); s.settimeout(1)
    try:
        s.connect(('127.0.0.1', 15702)); print('BRP up'); break
    except OSError:
        time.sleep(1)
    finally:
        s.close()
"
powershell -NoProfile -Command "(Get-Process ash_renderer).Id"

# 后台起的宿主窗口是最小化的：先还原再抓图（PostMessage 不碰用户鼠标键盘）
python -c "import ctypes;ctypes.windll.user32.PostMessageW(0x<HWND>,0x0112,0xF120,0)"

# 闭环：基线 → 改值 → 截图 → 相机 → 截图 → 复原
python tools/capture_window.py --pid <PID> --out .temp/01-baseline.png
python tools/brp.py set-transform <entity> --translation 0 0.5 0
python tools/brp.py camera --yaw-deg 75 --zoom 0.9
python tools/capture_window.py --pid <PID> --out .temp/02-after.png

# 虚拟鼠标拖拽环绕（3.13）：不碰真实鼠标，AI 亦可点选 egui 树行/面板
python tools/brp.py mouse-move --x 952.8 --y 369.6
python tools/brp.py mouse-button --button left --action press
python tools/brp.py mouse-move --x 792.8 --y 279.6
python tools/brp.py mouse-button --button left --action release
python tools/brp.py mouse-status

# 优雅退出取退出码
python -c "import ctypes;ctypes.windll.user32.PostMessageW(0x<HWND>,0x0010,0,0)"   # WM_CLOSE
```

注意三条：宿主必须先起（brp.py 连不上报 exit 2）；改值下一帧生效，紧随的截图不用等；`--host/--port` 是全局参数，写在子命令**前面**。
