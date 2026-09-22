# Plugin 与 PluginGroup 机制

> 2026-09-14 建。从《Bevy结构笔记》（机制问答）与《开发工具与语法笔记》（宏展开）收拢的 Plugin 专题。源码依据：`crates/bevy_app/src/plugin.rs`、`plugin_group.rs`、`app.rs`、`crates/bevy_internal/src/default_plugins.rs`。

## 一、Plugin：添加 = 执行 build

**核心机制**：`app.add_plugins(T)` 的实质动作就是同步调用 `T::build(app)`——证据在 `crates/bevy_app/src/app.rs:560`：`let f = AssertUnwindSafe(|| plugin.build(self));`（外层带 panic 捕获与 `plugin_build_depth` 防重入）。

**签名**：`fn build(&self, app: &mut App)`（`plugin.rs:59`）。接收者是 **`&self`**——插件别把状态存在自己身上，一切写入 `app`（World/资源/调度）；build 完插件本体只剩名字用于去重登记。

**生命周期四段**（`plugin.rs:59-83`）：

| 阶段 | 时机 | 用途 |
|---|---|---|
| `build` | add_plugins 时**立即**执行 | 插资源、注册系统、挂 SubApp |
| `ready` | 每帧 update 前检查 | 返回 false 可推迟进入下一阶段（异步资产/管线编译用） |
| `finish` | 全部插件 build 完、首帧前一次 | 跨插件依赖的初始化 |
| `cleanup` | finish 之后 | 收尾释放 |

finish/cleanup 由 runner 调用——`run_once` 里就是 `app.finish(); app.cleanup();`（`app.rs:1536`）。

## 二、Plugin 添加 Plugin：嵌套与组级

- **嵌套**：一个 Plugin 的 `build()` 里再 `app.add_plugins(别的插件)`；
- **组级**：`DefaultPlugins` 本身不是 Plugin，是 **PluginGroup**——`group.build()` 返回装满待建插件的 `PluginGroupBuilder`，而 **builder 自己又实现了 `PluginGroup`**（`plugin_group.rs:221`，`build(self) -> Self`），所以组/builder 都能整体喂给 `add_plugins`。

完整链条：

```
App::add_plugins(DefaultPlugins)
└─ DefaultPlugins::build()      ← 宏生成的：按 feature 条件把清单装进 builder（尚未执行任何插件）
   └─ App 拿到 builder（builder 也是 PluginGroup）
      └─ 逐条目执行 plugin.build(app)   ← 真正的"添加 = 执行 build"
```

## 三、DefaultPlugins 是怎么展开的（`plugin_group!` 宏）

### 3.1 清单与 `:::` 三冒号语法

`crates/bevy_internal/src/default_plugins.rs` 整个文件是宏输入，条目长成 `bevy_app:::PanicHandlerPlugin`。正常 Rust 路径里 `a:::b` 非法——这是 `plugin_group!` 宏（`crates/bevy_app/src/plugin_group.rs:106`）的匹配模式：

```rust
$($plugin_path:ident::)* : $plugin_name:ident
//  零或多个 `路径段::`，然后一个单独的 `:`，然后插件名
```

词法上 `:::` 切成两个 token：`::` + `:`。路径段数不定（1 段或 2 段），全用 `::` 宏就分不清路径边界——那个单独的 `:` 就是显式终结符。文件文档注释明说（`plugin_group.rs:79`）："If referencing a plugin within a different module, there must be three colons `:::`"。

| 写法 | 宏看到 |
|---|---|
| `bevy_app:::PanicHandlerPlugin` | 路径 `bevy_app::`（1 段）+ 分隔 `:` + 名字 |
| `bevy_render::pipelined_rendering:::PipelinedRenderingPlugin` | 路径 2 段 + 分隔 `:` + 名字 |

### 3.2 展开产物（手工还原）

```rust
/// This plugin group will add all the default plugins for a *Bevy* application:
///
///  - [`PanicHandlerPlugin`](bevy_app::PanicHandlerPlugin)
///  - [`LogPlugin`](bevy_log::LogPlugin) - with feature `bevy_log`
///  - ...（每条 bullet 由宏用 stringify! 拼出）
pub struct DefaultPlugins;          // unit struct，无字段——状态全在 build() 产物里

impl PluginGroup for DefaultPlugins {
    fn build(self) -> PluginGroupBuilder {
        let mut group = PluginGroupBuilder::start::<Self>();

        // bevy_app:::PanicHandlerPlugin —— 无 cfg 行，无条件加入
        {
            const _: () = { const fn check_default<T: Default>() {}
                            check_default::<bevy_app::PanicHandlerPlugin>() };
            group = group.add(<bevy_app::PanicHandlerPlugin>::default());
        }

        #[cfg(feature = "bevy_log")]
        {   // bevy_log:::LogPlugin
            …check_default::<bevy_log::LogPlugin>()…;
            group = group.add(<bevy_log::LogPlugin>::default());
        }

        // bevy_render::pipelined_rendering:::PipelinedRenderingPlugin —— 双段路径 + 双重条件
        #[cfg(feature = "bevy_render")]
        #[cfg(all(not(target_arch = "wasm32"), feature = "multi_threaded"))]
        {
            …;
            group = group.add(<bevy_render::pipelined_rendering::PipelinedRenderingPlugin>::default());
        }

        // ……其余条目同构，最后：
        group
    }
}
```

路径与名字分开捕获，各有四个用途：

1. 拼回正常路径生成代码：`group.add(<bevy_app::PanicHandlerPlugin>::default())`——**三冒号只活在宏模式里，展开后是普通 `::`**；
2. 生成文档条目：`" - [`PanicHandlerPlugin`](bevy_app::PanicHandlerPlugin)"`；
3. 转交 `#[cfg(feature = "...")]`（crate 名同时是 feature 名）；
4. `check_default::<插件>()` 编译期断言，强制每个插件实现 `Default`。

### 3.3 `#[custom(...)]`：剥壳重发

匹配器捕获 `#[custom($plugin_meta:meta)]` 里的内容，展开时**扔掉 `custom` 外壳、内部 meta 原样贴回**——源码里的 `#[custom(cfg(all(...)))]` 生成时变成真正的 `#[cfg(all(...))]`（见上面 PipelinedRenderingPlugin 的双重 cfg）。

为什么非要套一层：简单条件 `#[cfg(feature = "字面量")]` 有专门的匹配分支；**复杂条件**（`cfg(all(...))` 这类）匹配不上，必须用 `#[custom(<任意 meta>)]` 原样捕获。

### 3.4 链式调用：`set` / `disable` / `build`

不在宏里，在 `PluginGroup` trait（`plugin_group.rs:203`）和 `PluginGroupBuilder` 上：

- `DefaultPlugins.set(WindowPlugin{…})` —— trait **默认方法**（`:211`）：`self.build().set(plugin)`，即"先展开成 builder，再替换插件"；
- 关键一环：**`PluginGroupBuilder` 实现了 `PluginGroup`**（`:221`）——`.set()` 返回 builder 后还能继续 `.disable::<RenderPlugin>()`（`:501`）、`.set()`（`:312`），全程是 builder 在类型间流动；
- `add_plugins` 既收 PluginGroup 也收单个 Plugin；builder 因实现了 PluginGroup 而能直接喂入。

## 四、对本项目的意义（第二步直接用）

`DefaultPlugins.build().disable::<RenderPlugin>()` 的完整语义：

1. `build()`：按 feature 条件把全部插件**按声明顺序**装进 builder（内部是 `TypeId → PluginEntry{plugin, enabled}` 的表）；
2. `disable::<RenderPlugin>()`：按 **TypeId** 摘条目（与声明顺序无关），`enabled = false`；
3. `add_plugins(builder)`：遍历条目，只对 enabled 的执行 `plugin.build(app)`——**被禁插件根本不执行 build**，所以 RenderApp 不被创建、wgpu 不初始化。

自己的渲染插件 = 一个普通 `Plugin`：build() 里建 VkInstance/Device、插资源、注册系统（见入口篇第三节）。

## 相关

- 宏语法通用心法（怪符号先找 `macro_rules!`）在《开发工具与语法笔记》
- 结构总览（App/World/Schedule/SubApp）在《Bevy结构笔记》
