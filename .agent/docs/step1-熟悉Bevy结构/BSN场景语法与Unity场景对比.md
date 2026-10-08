# BSN 场景语法与 Unity 场景对比

> 2026-09-14 建。以 `examples/3d/3d_scene.rs` 为入口（9 行 main + 1 个场景函数，官方 BSN 语法示范），读透 `bevy_scene` 0.19.1 源码后整理。回答四个问题：**BSN 是什么 / 是什么场景格式 / 怎么用 / 够不够用**，最后与 Unity scene YAML 逐维度对比。本篇同时是资产系统（待深入清单 Q4）的**消费端**半篇——BSN 只讲"组件怎么进 World"，glTF 加载链路另算 Q4 主体。

## 0. 一句话定位

**BSN（Bevy Scene Notation）是"写"场景的语法，不是"存"场景的格式。** 0.19.1 里它只以 `bsn!`/`bsn_list!` 宏形态存在：编译期把 DSL 展开成普通 Rust 代码，构建 `Scene` 对象，运行时 spawn。官方规划的 `.bsn` 磁盘文件格式**尚未发布**（架构已就绪，见 §5）。

这与 Unity 立场相反：`.unity` YAML 是编辑器**序列化的产物**（人从来手写它），BSN 是**给人手写的源码**。这个差异贯穿全部对比，见 §6。

## 1. 解决什么问题：场景系统三挑战

`crates/bevy_scene/src/lib.rs:4-23`，任何场景系统都要过三关，BSN 的设计全部围绕它们：

1. **组合（Composability）**：小场景拼大场景，不重复共享常量——靠"场景函数包含 + 补丁"；
2. **字段级覆盖（Granular overrides）**：复用场景时只改一个字段（比如只改按钮宽度），不重抄整个组件——靠 **patching**；
3. **资产集成（Asset integration）**：场景里引用 mesh/纹理，不用手动接线 Handle——靠 `HandleTemplate` 自动解析。

## 2. 三层模型与两段式执行

### 2.1 类型分层

| 类型 | 定位 | 类比 |
|---|---|---|
| `Scene`（trait，`scene.rs:48`） | **单根实体**的描述："这个实体长什么样" | 一个 GameObject 及其组件 |
| `SceneList`（trait，`scene_list.rs:12`） | 多根列表，每项出一个实体 | 场景里的一排根物体（无公共父节点） |
| `ResolvedScene`（`resolved_scene.rs:165`） | **可 spawn 的产物**：模板列表 + 关系场景 + 缓存引用 | 序列化好的场景快照 |

官方心智模型（`scene.rs:21`）：`Scene : Entity` = `SceneList : Vec<Entity>`，两个 trait 是故意的类型区分，因为"场景缓存"只对单根有意义。

### 2.2 Template：超级 ECS 构造器

`bevy_ecs/src/template.rs:32`。`Template` 不是"模板代码"，是**延迟求值的组件构造器**：

```rust
pub trait Template {
    type Output;
    fn build_template(&self, context: &mut TemplateContext) -> Result<Self::Output>;
    fn clone_template(&self) -> Self;   // 支撑缓存/CoW
}
```

两个 blanket impl 是整座桥（`template.rs:390/404`）：

- `impl<T: Clone + Default> Template for T` —— 所以任何 `Default + Clone` 组件**自动可用**于 bsn!；
- `impl<T: Clone + Default> FromTemplate for T` —— `FromTemplate`（`template.rs:348`）给类型指定"canonical Template"。

需要 World 上下文的类型必须手写 FromTemplate + 专用 Template，两大常客：

- **`Handle<T>` ↔ `HandleTemplate<T>`**（`bevy_asset/src/handle.rs`）：路径/已有句柄/内联值三态，见 §3.2；
- **`Entity` ↔ `EntityTemplate`**（`template.rs:422`）：`#Name` 引用在 spawn 期才解析成实体 id。

`FromTemplate` 与 `Default` 同派生会冲突——derive `FromTemplate` 会生成伴生结构 `YourTypeTemplate`（自动实现 Default）来充当默认值（lib.rs:299-301）。

### 2.3 两段式：resolve → spawn

```
bsn! 宏展开的 Scene 对象们（纯数据，编译期构建）
   │  Scene::resolve() —— 把一串 Scene 的补丁按序归并进 ResolvedScene
   ▼
ResolvedScene { component_templates, bundle_templates, related, cached, entity_references }
   │  ResolvedSceneRoot::spawn/apply（resolved_scene.rs:47/65）
   ▼
逐个 template.build_template(&TemplateContext) → 组件写入实体
   └─ related 场景递归 spawn，用 Relationship（如 ChildOf）回连父实体
```

关键细节：

- **resolve 可失败**：依赖资产未加载时报 `MissingSceneDependency`（`scene.rs:159`）；**spawn 失败回滚**——中途出错 despawn 掉半成品实体（`resolved_scene.rs:47-57`）；
- 补丁落在 `ResolvedScene` 里：`get_or_insert_template::<T>` 拿到（或初始化）该类型模板再改字段（`scene.rs:340-349`），多个补丁按书写顺序叠加；
- `#Name` 引用的解析表是 `SceneEntityReferences`（`template.rs:95`），键 = 宏调用点 `(file, line, column)` + 名字 id（`template.rs:137-139`）——**每次宏调用一个作用域**，`bsn_list!` 的所有根共享同一作用域，兄弟可互引。

## 3. 3d_scene.rs 逐行解读

```rust
fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_systems(Startup, scene.spawn())   // ← 闭包变系统
        .run();
}

fn scene() -> impl SceneList {
    bsn_list! [
        (#CircularBase  Mesh3d(asset_value(Circle::new(4.0)))
                        MeshMaterial3d::<StandardMaterial>(asset_value(Color::WHITE))
                        Transform::from_rotation(Quat::from_rotation_x(-FRAC_PI_2))),
        (#Cube          Mesh3d(asset_value(Cuboid::new(1.0, 1.0, 1.0)))
                        MeshMaterial3d::<StandardMaterial>(asset_value(Color::srgb_u8(124, 144, 255)))
                        Transform::from_xyz(0.0, 0.5, 0.0)),
        (PointLight { shadow_maps_enabled: true }
                        Transform::from_xyz(4.0, 8.0, 4.0)),
        (Camera3d       template_value(Transform::from_xyz(-2.5, 4.5, 9.0).looking_at(Vec3::ZERO, Vec3::Y)))
    ]
}
```

一条条拆：

| 写法 | 机制 | 源码 |
|---|---|---|
| `bsn_list! [ (..), (..) ]` | 逗号分隔 = 各自独立实体；括号内空格分隔 = 同一实体的多个组件 | `scene_list.rs:88`（`EntityScene`） |
| `#CircularBase` | 插入 `Name("CircularBase")` 组件 + 在本作用域注册可引用名字 | `scene.rs:448-473`（`NameEntityReference`） |
| `asset_value(Circle::new(4.0))` | **内联资产**：`HandleTemplate::Value`——首次 build 时 `Assets::<Mesh>::add(circle)` 生成句柄，之后复用（值被 `take` 走，换成缓存句柄） | `handle.rs:381/341-374` |
| `Mesh3d(asset_value(..))` | `Mesh3d(pub Handle<Mesh>)` 派生 `FromTemplate`，其 Handle 字段的 canonical Template 就是 `HandleTemplate<Mesh>`；`Circle: Into<Mesh>` | `bevy_mesh/src/components.rs:41,102` |
| `asset_value(Color::WHITE)` | `Color: Into<StandardMaterial>`（`pbr_material.rs:943`），经 `asset_value` 的 `Into` 泛型隐式转换成材质资产 | `handle.rs:291` |
| `PointLight { shadow_maps_enabled: true }` | **单字段补丁**：只设阴影开关，其余字段（intensity、radius…）用类型默认值 | `bevy_light/src/point_light.rs` |
| `Camera3d` | 无值组件 = 整个用默认 | lib.rs 语法表 |
| `template_value(transform)` | 已构造好的组件**值**不能直接进 bsn!（宏要的是"场景变量"），包一层变成全量覆写补丁 | `scene.rs:288-297` |
| `scene.spawn()` | `SpawnListSystem`（为 `FnMut() -> impl SceneList` 实现）：闭包包成 `FnMut(&mut World)` 系统体，Startup 跑一次即 `world.spawn_scene_list(self())` | `spawn_system.rs:23-35` |

两个容易忽略的点：

1. **为什么 Startup 里 immediate spawn 能成功**：四个实体全用内联资产（`Value` 态），无路径依赖、无需异步加载，`World::spawn_scene` 同步 resolve + spawn 不报错。若换成 `Sprite { image: "player.png" }` 这种路径引用，immediate 可能撞上"未加载"，就得走 `queue_spawn_scene`（§4）。
2. **这四个 `#Name` 没有被引用**，纯当调试名/层级标记用。真实价值在互引：`Link(#Right)` 这类把实体 id 塞进组件字段（需 `FromTemplate`）。

## 4. 使用方式全景

### 4.1 Spawn API 矩阵（`spawn.rs`）

| 入口 | 立即（immediate） | 排队（queued） |
|---|---|---|
| `World` | `spawn_scene` / `spawn_scene_list` | `queue_spawn_scene` / `queue_spawn_scene_list` |
| `Commands` | `spawn_scene`（命令应用时执行） | `queue_spawn_scene` |
| 实体 | `apply_scene`（覆写到已有实体） | `queue_spawn_related_scenes::<Children>`（作为关系的子级挂入） |

- **immediate**：当场 resolve + spawn，依赖未加载直接报错；
- **queued**：把场景存成 `ScenePatch` 资产、注册依赖，等 `AssetEvent::LoadedWithDependencies` 后落地。落地时机 = **SpawnScene 调度**（Update 与 PostUpdate 之间——与《Bevy结构笔记》已核实的调度顺序一致，`lib.rs:100-103` 文档明说）；
- `ScenePlugin::build`（`lib.rs:943-958`）注册 `QueuedScenes`/`WaitingScenes` 资源、`ScenePatch`/`SceneListPatch` 资产类型、以及 SpawnScene 里的 `(resolve_scene_patches, spawn_queued).chain()`。**bevy_scene 不依赖 bevy_render**（《DefaultPlugins分类.md》已核实），纯数据 crate。

### 4.2 语法速查（完整表见 `macros/src/lib.rs:38-79` 文档）

| 前缀 | 条目 | 效果 |
|---|---|---|
| （无） | `Comp` / `Comp(v)` / `Comp { f: v }` | 插入组件，未写字段走默认（补丁语义） |
| `:` | `:scene()` / `:"a.bsn"` | 引入**缓存**场景（CoW，须为首条目；函数场景缓存未实现） |
| `#` | `#Player` | 命名实体 + 注册引用 |
| `@` | `@MySceneComp { @prop: 1, field: 2 }` | `SceneComponent`：组件自带场景；`@field` 是 props（传给场景函数），普通字段是组件本身 |
| `~` | `~MyTemplate { .. }` | 直接用自定义 `Template` 类型（绕开 FromTemplate 分派） |
| `on()` | `on(\|e: On<Damage>\| {...})` | 给实体挂实体观察者（EntityEvent） |
| `Children [...]` | 关系列表 | 任意 `RelationshipTarget` 都行，不止 Children |
| `{expr}` | 值/场景表达式 | 嵌任意 Rust 表达式、Scene/SceneList |

组件入场的门槛（lib.rs:286）：**派生 `Default + Clone`（首选）或 `FromTemplate`**。枚举特殊：要求每个 variant 有默认（配 `VariantDefaults` 伪派生），补丁可换 variant 并合并字段。

### 4.3 SceneComponent：组件与场景绑死

`SceneComponent`（`scene_component.rs:13`）= `Component + FromTemplate` + 关联 `Props` + `fn scene(props) -> impl Scene`。用 `@Player { score: 0 }` 语法引入时，**组件本身和它的整棵场景一起 spawn**——系统查到 `Player` 组件即可假设配套场景都在。它和 Required Components 的取舍（lib.rs:838-861）：SceneComponent 是"层级化、依赖感知、可补丁、只在 spawn 期生效"；Required Components 是"扁平、即时、不可补丁、处处生效"。含层级/依赖/要 World 的用前者，纯平铺初始化用后者。

### 4.4 BSN 场景 vs Commands 在 Setup 直接构造（2026-09-18 补）

问答沉淀："用 `bsn!` 生成场景"和"在 setup 里用 `Commands.spawn` 生成场景"有什么区别。结论先行：**两者最终都把组件写进 World，区别不在"能不能做到"，而在组件值在哪里构造、依赖从哪来、怎么表达差异**——BSN 是更高层的声明式入口，`Commands.spawn` 是底层命令式入口，BSN 落地仍走 Commands（`scene.spawn()` 就是把 `fn() -> impl SceneList` 包成 `FnMut(&mut World)` 系统塞进 Startup）。

| 维度 | Commands + Setup | BSN |
|---|---|---|
| 组件构造 | 手写完整值 + `..Default::default()` | 补丁语义：只写差异字段，其余走类型 `Default` |
| 资产引用 | 必须 `Res<AssetServer>` / `ResMut<Assets<T>>` 拿句柄逐层传参 | Handle 字段直接写 `"player.png"` 路径，或 `asset_value` 内联注册，**依赖不外泄** |
| 嵌套/层级 | `children![]` 可内联，但子实体的资产依赖要一路穿层传递 | `Children [ ... ]` 内联，子场景自带依赖解析 |
| 实体互引 | 手动 `.id()` 接线，顺序敏感 | `#Name` 作用域引用，spawn 期 `EntityTemplate` 解析 |
| 观察者 | `.observe(...)` 链式调用 | `on(\|press: On<Pointer<Press>>\| {...})` 写在场景里 |
| 组合复用 | 函数返回 Bundle，参数手动传 | 场景函数 + 补丁叠加即变体 |
| 失败语义 | 构造值阶段失败=编译错误，运行期基本不半途失败 | `spawn_scene` 依赖未加载会报错（改用 queued 等待）；spawn 中途出错回滚半成品实体 |
| 缓存 | 每次全量构造 | `:"a.bsn"` 场景缓存（CoW），重复实例化只 resolve 顶层补丁 |

关键洞察（release note `next-generation-scenes.md` 的官方对比）：老写法的本质痛点是**依赖外泄**——bundle 函数必须知道自己内部用了什么资产，把 `&AssetServer` 一路传进嵌套结构；BSN 靠 Template 能在 spawn 期访问 World，场景函数签名完全不需要资产参数。**类型安全两者打平**（都是 Rust 代码，编译期全查），BSN 的类型优势是相对文本场景格式而言。选型：一次性几行实体的简单 setup 用 Commands 少一层间接；有资产引用、深层嵌套、需要复用变体的场景（尤其 UI）用 BSN。

## 5. 功能盘点：建场景要的能力，BSN 有没有

| 能力 | 状态 | 说明 |
|---|---|---|
| 层级/任意关系 | ✅ | `Children []` 及任何 `RelationshipTarget`（`MyRel []`） |
| 组合复用 | ✅ | 场景函数 + 包含 + 补丁；元组场景也实现 `Scene` |
| 字段级覆盖 | ✅ | 补丁按序归并，未写字段保留 |
| 资产引用 | ✅ | 路径字符串（Handle 字段直接写 `"a.png"`）+ 内联 `asset_value` |
| 观察者/事件逻辑 | ✅ | `on()` 挂实体观察者，闭包可捕获环境变量 |
| 参数化/动态值 | ✅ | 场景函数即 Rust 函数：参数、`{expr}`、props |
| 循环生成 | ✅ | `Vec<Scene>`/`impl SceneList` 集合 + `{items}` 注入 |
| 条件分支 | ⚠️ | 无内建 if/match，用 `Box<dyn Scene>` 变通（lib.rs:569-588 官方模式） |
| `.bsn` 磁盘文件 | ❌ 未发布 | 架构已备好：`ScenePatch` 本身是 Asset（`init_asset::<ScenePatch>`），`:"a.bsn"` 语法已通；测试里用 FakeSceneLoader 演示第三方格式接入（lib.rs:1133-1151）。发布前用 glTF 或纯宏 |
| 场景缓存 | ⚠️ | 仅资产场景（`:` 前缀，CoW）；函数场景/SceneComponent 缓存未实现（lib.rs:359-364） |
| 热重载 | ❌ | 随 `.bsn` 格式一起未来才有（资产系统本体支持，差格式） |
| 编辑器/可视化 | ❌ | 无；rust-analyzer 支持（补全/跳转/hover，lib.rs:29-30）是当前的"编辑器" |
| 多场景叠加 | ✅ | 就是多次 `spawn_scene_list` / `apply_scene`，无 additive 特殊概念 |

**结论：写场景（宏形态）功能闭环，够用；管场景（磁盘格式、编辑器、热重载）0.19 还不存在。** 官方自己的建议（lib.rs:877-878）：现阶段外部内容用 glTF，场景组织用 `bsn!` 宏。

## 6. 对比 Unity scene YAML

先对齐 Unity 侧事实：`.unity` 是编辑器**序列化产物**（ForceText 模式下），一个 YAML 多文档流，每序列化对象一个 document（`!u!1 &100000 GameObject`、`!u!4 Transform`、`!u!114 MonoBehaviour`...），对象间用 fileID 互引，外部资产用 GUID（meta 文件）+ fileID。它由编辑器读写，人基本不手碰。

### 6.1 本质立场对比

| | Unity scene YAML | Bevy BSN (0.19.1) |
|---|---|---|
| 本质 | 运行时**序列化格式**（编辑器存档，可逆） | 编译期**书写语法**（宏展开为构建 `Scene` 的 Rust 代码） |
| 谁来写 | 编辑器（人通过 Inspector 间接写） | 程序员直接手写（rust-analyzer 辅助） |
| 完整性 | 全字段序列化，所见即所得 | **补丁式**：只写差异，缺省走类型 Default |
| 写错会怎样 | 静默吞掉/警告：字段名打错 = 值被忽略，运行时才暴露 | 编译错误：组件名、字段、类型错都过不了编译 |
| 逻辑附着 | MonoBehaviour 挂场景里（数据+行为同文件，序列化字段） | 数据（Component）与逻辑（System/Observer）分离；`on()` 只挂观察者闭包 |

### 6.2 关键机制逐项对比

| 维度 | Unity | BSN | 取舍 |
|---|---|---|---|
| **组合复用** | Prefab：独立资产文件，嵌套 prefab + override 链，改基础 prefab 全量传播 | 场景函数包含 + 补丁就地归并（`enemy()` + `Health { max: 200 }`）；SceneComponent 是"组件级 prefab"，还带 props 参数化 | Unity 的 override 链深了难追溯；BSN 补丁就地展开，等价于"prefab 塌平进场景"，简单直接，但没有独立 prefab 文件（等 `.bsn` + `:` 缓存组合补齐） |
| **实体引用** | fileID：序列化器保证，编辑器维护 | `#Name`：每次宏调用一个作用域，spawn 期经 `EntityTemplate` 解析成真 id；`bsn_list!` 根间可互引 | fileID 稳定持久（跨会话不变），`#Name` 是 spawn 期临时解析——**组件里必须存解析后的 Entity，不能存 EntityTemplate**（lib.rs:216-219） |
| **资产引用** | GUID + fileID（meta 文件体系，移动文件不丢引用） | 路径字符串（`"player.png"` → `HandleTemplate::Path`，AssetServer 加载去重）或内联值（`asset_value`） | Unity 引用稳但需 meta 生态；BSN 靠 AssetServer 路径 + 依赖追踪，路径即契约，无 GUID 层 |
| **字段默认值** | 无默认概念——序列化时全部写死 | 类型 Default + 补丁归并，`PointLight { shadow_maps_enabled: true }` 一行只动一个字段 | BSN 大幅省字数、diff 干净（宏文档明说这是设计目标之一，lib.rs:26-28）；代价是"默认值藏在代码里"，看场景文件推不出完整状态 |
| **类型安全** | YAML 弱：改名/换类型静默失效 | 宏展开是合法 Rust，类型全查；隐式 `Into` 转换受控（`Color`→`StandardMaterial`、`TextSize`→`FontSize`） | 这是 BSN 相对一切文本场景格式的最大优势 |
| **版本管理** | YAML 本来就是为可 diff 设计的 | BSN 住 `.rs` 里，同样可 diff；极简语法（无标点噪音）也是为 merge 友好 | 打平，且 BSN 多了编译期兜底 |
| **运行时修改** | 实例化后改属性要走 API；prefab 回写靠编辑器 | spawn 出来就是普通实体+组件，直接 ECS 操作；程序化补丁有 `PatchFromTemplate`/`PatchTemplate` | Bevy 侧"改场景"与"改实体"是同一套 API，无特殊场景层 |
| **多根/子场景** | 一个 .unity 文件一个场景，层级固定单根（多场景靠 additive 加载） | `bsn_list!` 天然多根；`{expr}` 把 SceneList 注进 Children | BSN 的组合粒度更细（函数级），Unity 粒度是文件级 |
| **可视化编辑** | 编辑器全功能（场景图、Inspector、prefab 变体） | 无；规划中的 `.bsn` 格式声明与宏语法兼容，未来编辑器生态才有落点 | 目前 Bevy 这边靠代码 + LSP，实质是"程序美术"路线 |
| **手写体验** | 不可行（格式为机器往返设计，fileID/GUID 手写即灾难） | 可行且舒适——这就是 BSN 的立项理由 | 两者根本不同取向的产物 |

### 6.3 一句话总结

**Unity YAML 是编辑器的存档格式，BSN 是程序员的书写语言。** 前者为"编辑器 ↔ 磁盘 ↔ 运行时"的往返保真设计（全量序列化、GUID 稳定引用、fileID 编织）；后者为"人直接写、编译器把关、组合即代码"设计（补丁省字、`#Name` 作用域引用、任意 RelationshipTarget、Rust 表达式内嵌）。BSN 目前缺的是 Unity 那套"管"的一侧——磁盘格式、编辑器、热重载，全在官方路线图上；而 Unity YAML 永远不会给你的编译期检查，BSN 从第一天就有。

映射到熟悉概念收尾：Unity 里"场景 = Prefab 的容器，实例化时应用 override 链"；BSN 里"场景 = Scene 对象的归并，resolve 时补丁全塌平，spawn 时模板逐个求值"。你过去对 prefab override 的直觉可以整体平移到"包含 + 补丁"上，只是层级变浅、检查变严。

## 7. 对本项目（宿主 + 自研渲染器）的落点

1. **对渲染链路零侵入**：BSN 只是"组件进 World 的入口之一"，spawn 完就是普通实体。第三步采集系统照常 `Query<(&Mesh3d, &Transform)>` 直读——不关心实体从哪来（BSN 宏、glTF、手写 spawn 一律平权）。
2. **`Assets<StandardMaterial>` 在主世界**（PbrPlugin 注册，已核实于《DefaultPlugins分类.md》）——M2 取数时直接 `Res<Assets<StandardMaterial>>` 可拿到 `asset_value` 塞进来的材质数据。
3. **顺带发现**：`StandardMaterial` 自带 `#[bindless(index_table(range(0..31)))]`（`pbr_material.rs:19-22`，AsBindGroup 派生属性）——官方 PBR 材质的 bindless 绑定形态现成可抄：索引表 range + 材质数据 binding_array，与我们 M2/M3 的 bindless 池思路同构，值得回读 `AsBindGroup` derive 展开结果。
4. **M1 后的第一个可跑 3D 例程**：`3d_scene.rs` 无需禁渲染即可运行，可作为第二步宿主壳的对照组（Bevy 原生渲染长什么样、帧率如何）。

## 8. 遗留与下一步

- **Q4 主体未动**：资产系统全链路（AssetServer 异步加载、AssetEvent、loader 注册）与 glTF→Mesh3d 取数链路——本篇只覆盖了"资产如何被场景消费"这一小段；
- **`.bsn` 文件格式**：随版本升级跟进（本文 API 条目基于 0.19.1 核实）；
- **函数场景缓存**：未实现，性能敏感的动态生成场景先自行 memoize。

## 概念盘点（自查）

Scene / SceneList / ResolvedScene / ScenePatch（缓存载体，本身是 Asset）/ Template / FromTemplate（派生产出 XxxTemplate 伴生结构）/ HandleTemplate（Path/Handle/Value 三态）/ EntityTemplate（#Name 引用的编译期形态）/ SceneEntityReference（作用域键 = 调用点+名字）/ SceneComponent（组件+场景绑死，@语法，props）/ patching（字段级补丁归并）/ SpawnScene 调度（Update 后 PostUpdate 前落地）/ on()（实体观察者入场）/ asset_value vs template_value（资产句柄模板 vs 组件值全量覆写）。
