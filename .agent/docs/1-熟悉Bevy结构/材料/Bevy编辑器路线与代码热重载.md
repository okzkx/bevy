# Bevy 编辑器路线与代码热重载

> 2026-09-14 建。三轮讨论整理：①"Bevy 启动后直接运行游戏 vs Unity Editor/Play"；②"为什么不做 Editor Mode"；③"替换 DLL 不就行了"。全部 0.19.1 源码事实已核实（标注 file:line），立场性结论单独标明。

## 0. 先回答字面问题：启动即运行，没有模式切换

**`cargo run` 起来的那一刻就是游戏本体在跑**——没有 Editor Mode、没有 Play 按钮，进程一生 = 游戏一生。机制根源在 runner（runner 是唯一循环主人，见《Bevy结构笔记》§3）：Unity 是"一个进程两种循环"（编辑器循环 + Play 循环，按钮切换）；Bevy 只有游戏循环一种。

**根本差异：双真身 vs 单真身**：

| | Unity | Bevy |
|---|---|---|
| 磁盘真身 | 场景/资产有序列化产物（YAML） | 只有源码 + 原始资产文件 |
| 内存真身 | 加载后的对象图（Play 时再实例化一份） | World——**唯一**的活态 |
| Stop 后 | 丢弃 play 改动，恢复编辑态 | 进程退出，一切蒸发 |
| 改场景 | 编辑器里改，写回序列化文件 | 改源码（BSN）/资产文件，下次运行生效 |

Unity 双真身的副产品是"play 改动不落盘"（所以有 apply 回 prefab 这类操作）；Bevy 单真身没有"要不要保存回场景"的问题——可保存的东西（代码、资产）本来就在磁盘上。

**"编辑器专有代码"的表达替换**：`UNITY_EDITOR` 宏隔离 → cargo feature + `debug_assertions`。例证：`SceneComponentInfo` 的 on_add 校验钩子带 `#[cfg(debug_assertions)]`（`scene_component.rs:23`，"场景组件不许裸 spawn"报错仅 debug 构建存在）；`ValidateParentHasComponentPlugin`（VisibilityPlugin 挂的）同类 dev 护栏。

**对本项目的意义**：跑的 = 发布二进制（无 editor/play 行为差异坑）；迭代节奏 = 改代码重编译重启（`dynamic_linking` 缩链接时间）或改资产 watch 热重载不重启；Unity 每次 Play 进出的 domain reload + 场景重载延迟，Bevy 每进程只有一次 World 构建。

## 1. 再修正深层前提：不是不做，是次序反了

Bevy 的立场是**引擎先于编辑器**（editor-last），不是不要编辑器：

- 官方编辑器在独立仓库 `bevy_editor_prototypes` 原型阶段，**未并入本仓库**（核实：`crates/` 里只有 `bevy_dev_tools`/`bevy_diagnostic`，无 editor crate）；
- 编辑器的基建已在 0.19 主干进场：
  - **BSN**（0.19 主打，见《BSN场景语法与Unity场景对比.md》）——场景数据模型定型，编辑器有东西可编辑；
  - **BRP**（`bevy_remote` crate）：JSON-RPC 2.0 over HTTP，外部客户端可**检视并修改**运行中 World 的 ECS 状态（`bevy_remote/src/lib.rs:1-5`）——编辑器对 Bevy 来说只是"一个远程客户端"；
  - `bevy_dev_tools`：infinite grid 文档原话 "suitable as a ground plane for editors"；
  - rust-analyzer 深度支持（bsn! 宏内补全/跳转/hover，bevy_scene lib.rs:29-30）是当前的"编辑器"。

## 2. 为什么 Editor Mode 现在做不了：三座山

### 山一：Rust 语言本身（最硬）

Unity Editor Mode 的真正能力 = ①运行中改任何序列化数据；②改 C# 代码热重载（Mono JIT + Editor 程序集重编译）。①Bevy 正在补（bevy_reflect 反射重建 + BSN 补丁系统）；②Rust **没有答案**——无稳定的运行时代码加载（详见 §3 三堵墙）。这决定了 Bevy 的编辑器形态只能是"数据编辑器"（.bsn 资产、组件值），不可能是"代码热编辑环境"。

### 山二：架构立场——引擎是库，编辑器是房客

editor-first 引擎的数据模型会围着序列化格式转（Unity YAML 全量序列化、GUID 体系、fileID 编织全是"编辑器是宇宙中心"的产物，见《BSN场景语法与Unity场景对比》§6）。Bevy 反过来：先让 ECS/资产/场景地基定型，**编辑器与游戏代码平权，不设特权通道**——编辑器能做的事代码都能做。BRP 是这个立场的体现。

### 山三：人力与返工风险

小维护者团队，编辑器是多年级工程；基础设施（ECS→渲染→资产→场景）没定型就动工 = 每次引擎改版编辑器重做。BSN 0.19 才落地——官方显然认为"场景数据模型"这块最后的地基刚打好。

## 3. "替换 DLL 就行了"——三堵墙

DLL 替换机制本身在原生世界完全可行（`LoadLibrary/FreeLibrary`、`libloading`），游戏界玩了几十年。但 Unity 那种"任意改 + 状态无损衔接"的体验，靠的是三件原生世界没有的东西：

| 墙 | 内容 | Unity 为什么能过 |
|---|---|---|
| **状态布局迁移** | 老堆内存按老 struct 布局摊开，改字段 = 全部错位；原生无元数据可迁移 | Mono 托管运行时 + 全量序列化系统跨 domain reload 搬运（代价：非序列化字段/static 重置——Unity 程序员的日常痛） |
| **类型同一性** | Rust 无稳定 ABI：同一 struct 在宿主和新 dylib 里是**两个类型**（TypeId/vtable/单态化都不同），跨边界传递是 UB；而 Bevy ECS 到处拿类型当 key（ComponentId、archetype 布局、TypeRegistration）——DLL 一换要么写"热 schema 迁移器"，要么整个 World 重建 | 托管世界类型有元数据身份，domain reload 天然重建 |
| **活着的代码指针** | 换 DLL 瞬间，所有指进老代码段的指针悬空。Bevy 里满地都是：system 函数指针存 Schedule、`Box<dyn Fn>` Command 队列、`SystemHandle`、**BSN `on()` 闭包作为 Observer 直接存进 World**（scene.rs 的 `OnTemplate`） | domain reload 重建一切托管对象 |

## 4. 原生热重载的真实光谱（都活在墙下）

| 方案 | 能做什么 | 天花板 |
|---|---|---|
| Handmade Hero 式 cdylib（`hot-lib-reloader` crate，有 Bevy demo） | 逻辑 crate 整体重载，状态过一道手写 state 结构 | 布局冻结（改 struct 即作废）、边界禁 trait 对象、demo 级 |
| Unreal Live Coding（工业级参照） | 替换函数地址，改函数体即时生效 | **头文件改动（改 struct/加字段）必须重启**（官方文档明说）——原生天花板 = 函数体可换、数据布局不可动 |
| Dioxus subsecond/hotpatch（Rust 前沿，2025） | 补丁 dylib + 符号重定向，Rust 里最接近"即时生效" | 同样布局冻结，函数补丁级 |
| 脚本层（Lua/Rhai/WASM，`bevy_mod_scripting` 等） | 逻辑真正热换 | 在 Rust 游戏里开"小托管区"——反证 Rust 本体做不到 |

**修正后的结论**：原生世界有函数补丁级热替换，但没有"任意改 + 状态无损"的 Editor Mode 体验——那份体验的学费是托管运行时 + 全量序列化 + 可卸载代码域，Rust 三样皆无且与设计立场正面冲突。

## 5. Bevy 的回答：数据热、逻辑冷

| 侧 | 热度 | 机制 |
|---|---|---|
| 资产 | ✅ 现在 | `AssetPlugin` watch 热重载 |
| 场景数据（.bsn） | 未来 | 文件格式未发布，架构已备（《BSN场景语法》§5） |
| 组件值 | ✅ 现在 | BRP 运行中改 |
| 逻辑（system/observer） | ❌ 冷 | 编译进二进制，改 = 重编译重启 |

注意 `dynamic_linking` feature 缩的是**链接时间**，不是热重载，别被名字骗。

## 6. Editor-less 对本项目反而是特性

把 Bevy 当**无头宿主嵌入**（本项目核心用法）在 Unity 里做不到——Unity 引擎死绑"编辑器/Player"两种壳。Bevy 当库嵌入、BRP 起个 HTTP 端点即可远程检视 World（调试面板可用）；将来官方编辑器编辑的是 `.bsn` 资产，经 `ScenePatch` 资产管道进 World，与自研渲染器完全正交。

一句话：**Unity 的编辑器是引擎的母亲（先有它才有一切），Bevy 的编辑器是引擎的房客（用公开 API 的一个应用）**——房客还没搬进来，是因为房子（ECS/资产/BSN 地基）2026 年才封顶。
