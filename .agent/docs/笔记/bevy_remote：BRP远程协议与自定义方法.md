# bevy_remote：BRP远程协议与自定义方法

> 定位：**侦察篇**——2026-09-22 用户问"Bevy 有没有工具能完全展现 World 里所有 Entity 和 Component"，顺藤摸到 `bevy_remote` 并通读源码。**未来 Web Console 的候选底座**：本篇先存档结论，开工时在此基础上展开，不重复侦察。
> ⚠ 时效警告：0.20 把方法名从旧版 `bevy.query` 风格**整体改名**为 `world.*` 系列，网上旧资料照抄会得到 METHOD_NOT_FOUND；本篇全部核对于本仓库 0.20.0-dev（`crates/bevy_remote/`）。

## 1. 一句话定性

**不是 CLI 工具，是插件库。** `RemotePlugin`（协议核心）+ `RemoteHttpPlugin`（HTTP 传输）两个插件装进 App，在**自己的进程内**起 HTTP 服务器，World 通过 JSON-RPC 2.0 暴露在 `127.0.0.1:15702`。客户端随便——curl、PowerShell、网页、第三方 GUI 都行。Bevy 仓库自带一个 158 行的 Rust 命令行客户端**示例**（`examples/remote/client.rs`），那是演示不是发布工具。名字相近的 `bevy_cli` 是社区另一个项目（脚手架/web 构建），与此无关。

对 App 的意义：**引擎变成可选暴露的"ECS 服务端"**——远程可对实体、组件、资源增删改查，还能订阅变化（`+watch`）和内省引擎自身（schema、调度器图）。

## 2. 架构两层：协议与传输解耦

**协议核心 `RemotePlugin`**（`lib.rs:596`）：维护方法注册表 `RemoteMethods`（`HashMap<方法名, handler>`，`lib.rs:1047`）。handler 本身**就是一个 Bevy system**：`SystemId<In<Option<Value>>, BrpResult>`——入参是请求 `params` 的 JSON，返回值序列化成响应。请求处理跑在专门的 `RemoteLast` schedule（`RemoteSystems::ProcessRequests`，`lib.rs:993/999`），即 handler 在真实 World 上按帧节拍执行：天然线程安全，代价是**延迟以帧为单位量化**（insert 的请求下一帧生效）。

**传输 `RemoteHttpPlugin`**（`http.rs:113`）：hyper + smol 实现的 HTTP 服务，`Startup` 时启动。默认 `127.0.0.1:15702`（`http.rs:52`）；开 `bevy_render` 时 RenderApp 里另起一台，默认 15703，专查渲染子世界。三个关键行为：

- **RenderApp 不存在则静默跳过**（`get_sub_app_mut(RenderApp) else return`）→ 禁渲染宿主零障碍兼容；
- WASM 没有 HTTP 传输（门面 Cargo.toml 用 target 配置处理）；
- 协议/传输解耦，可以自写其它传输（官方只内置 HTTP）。

请求格式标准 JSON-RPC 2.0（`BrpRequest`，`lib.rs:1122`）；响应带 `result` 或 `error`，错误码 = 标准 JSON-RPC 段 + 自定义段（如 `-23401 ENTITY_NOT_FOUND`，`lib.rs:1469` 起）。

## 3. 内置方法全景（0.20 命名，`builtin_methods.rs:46-121` 常量）

| 分组 | 方法 | 说明 |
|------|------|------|
| 实体·读 | `world.query` | with/without 过滤 + 取组件值；`option: ["all"]` 一次抓全部反射组件 |
| | `world.list_components` | 列全部组件类型名（不需反射） |
| | `world.get_components` | 按 entity 取值，`strict` 控制缺组件时报错还是回 errors |
| 实体·写 | `world.spawn_entity` / `insert_components` / `remove_components` / `mutate_components` / `despawn_entity` / `reparent_entities` | 远程改 World 全套 |
| 事件/消息 | `world.trigger_event` / `world.write_message` | 远程触发 observer 事件、写消息 |
| 资源 | `world.list_resources` / `get_resources` / `insert_resources` / `remove_resources` / `mutate_resources` | 资源同款读写 |
| 订阅流 | `world.get_components+watch` / `list_components+watch` / `observe+watch` | watching 型 handler：无变化返回 None 不响应，有变化才推——远程订阅 |
| 元信息 | `registry.schema` / `rpc.discover` / `app.info` / `diagnostics.list` · `get` / `schedule.list` / `schedule.graph` | 反射类型 schema、可用方法发现、诊断、调度器结构 |

## 4. 反射边界："完全展现"的口径

- **存在性查询永远全量**：`list_components`、`query` 的 with/without/has 过滤只按全限定类型名匹配，不依赖反射——自研组件没实现 Reflect 也能列出/过滤。
- **取值/写值走 `bevy_reflect`**：组件需 `#[derive(Reflect)] #[reflect(Component)]` + `app.register_type::<T>()` 才能查到值，否则 `get_components` 回 errors。
- 绕反射拿存在性：`query` 的 `has` 模式返回布尔。
- 对照：`World::archetypes()` 遍历（`bevy_ecs/src/world/mod.rs:255`，组件名经 `ComponentInfo::name()`→`DebugName`）是唯一**反射无关**的全量口径；资源用 `world.iter_resources()`（`world/mod.rs:3652`）。BRP 管交互式查询，archetype 遍历管离线对账，互补。

## 5. 自定义方法：handler 就是 system（本篇重点）

**构建期**（`RemotePlugin::with_method_main`，`lib.rs:615`）：

```rust
impl Plugin for AshHostPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            RemotePlugin::default()
                .with_method_main("ash_renderer/scene_stats", scene_stats),
            RemoteHttpPlugin::default(),
        ));
    }
}

// handler 就是 system：入参 = 请求 params JSON，出参 = 响应 JSON
fn scene_stats(
    In(_params): In<Option<Value>>,
    stats: Res<SceneArrivalStats>,      // 自家插件维护的资源
) -> BrpResult {                        // BrpResult<T=Value> = Result<T, BrpError>（lib.rs:1510）
    Ok(json!({ "entities": stats.entity_count, "images": stats.image_count }))
}
```

**运行期**（`RemoteMethods` 是公开 Resource，`lib.rs:1058`）：任何 system 里 `world.register_system(handler)` 拿 `SystemId`，包成 `RemoteMethodSystemId::Instant` 后 `insert`——可按条件/运行中动态增删方法。运行期插入的方法同样被 `rpc.discover` 自动发现（`builtin_methods.rs:1146` 直接读该资源）。

handler 能力上限：**可独占 `&mut World`**——内置的 `process_remote_query_request(In(params), world: &mut World)` 就是独占系统（`builtin_methods.rs:920`）。所以自定义方法可读写任意状态、调插件公开函数、发事件/消息、`commands.run_system` 触发已注册系统——相当于给插件开了一个网络可达的控制端点，不止"查"。

细节：方法名任意字符串（内置点分风格，自定如 `ash_renderer/xxx`）；重名静默替换旧 handler；未知方法 `-32601`；`with_method_render` 注册进渲染子世界注册表——宿主无 RenderApp，用不上。

## 6. Web Console 落点（未开工，先记结论）

1. **BRP 就是现成后端**：网页 `fetch` POST JSON-RPC 即可读写 ECS，不用自建任何服务器。
2. **跨域已留口**：`Headers`/`HostHeaders` 资源给 HTTP 响应加头，源码 doc 里就有 `cors_headers` 示例（`http.rs:184-194`）——`RemoteHttpPlugin::default().with_headers(Headers::new()...)`。⚠ CORS 预检（OPTIONS）是否完备**未验证**，开工先测。
3. **安全边界**：默认只绑 loopback、**完全无鉴权**——console 只能开发期本地用，绝不 `with_address` 绑 `0.0.0.0` 对外。
4. **自定义方法当"业务接口"**：内置方法是裸 ECS CRUD，让前端拼反射类型名很脆；渲染器内部状态（bindless 池占用、FlightHelmet 到货统计、collect_scene 采集范围）应注册成自定义方法暴露——前端只调语义接口。
5. **Unity 类比**：相当于把 Unity Editor attach 到 Player 的通道（Hierarchy/Inspector 走网络读运行时状态）协议化、无头化。官方 Bevy Editor 将来大概率骑在 BRP 上——见《[Bevy编辑器路线与代码热重载](../1-熟悉Bevy结构/材料/Bevy编辑器路线与代码热重载.md)》（该篇 §"BRP 基建现状"是编辑器路线视角，协议细节以本篇为准；其"调试面板可用"的判断就是本篇 Web Console 落点的出处）。

## 7. 启用成本与源码索引

**feature**：门面 `bevy` 加 `features = ["bevy_remote"]`（原生平台带 http + bevy_asset + bevy_render；宿主本就有 asset/render crate，增量只有 hyper/async-io/serde_json 等小依赖）；`ash_renderer` 侧另需直接依赖 `serde_json`（取 `Value`）。

| 位置 | 内容 |
|------|------|
| `bevy_remote/src/lib.rs` | 协议核心：`RemotePlugin`:596、自定义方法:615-666、内置注册 build():889-919、`RemoteLast`/`RemoteSystems`:993/999、`RemoteMethods`:1047、`BrpRequest`:1122、`BrpError`/error_codes:1386/1469、`BrpResult`:1510 |
| `bevy_remote/src/http.rs` | 传输：`DEFAULT_PORT`:52、`Headers`:68、`RemoteHttpPlugin`:113、CORS 示例:184-194 |
| `bevy_remote/src/builtin_methods.rs` | 方法常量:46-121、独占 handler 样例:920、`rpc.discover` 实现:1146 |
| `examples/remote/` | `server.rs`（91 行最小服务端）· `client.rs`（158 行 Rust 客户端示例）· `integration_test.rs` |

**启用就两行**：`app.add_plugins((RemotePlugin::default(), RemoteHttpPlugin::default()));`
