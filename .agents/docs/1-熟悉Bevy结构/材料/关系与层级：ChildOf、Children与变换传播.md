# 关系与层级：ChildOf、Children 与变换传播

> 2026-09-21 完成，待深入清单最后一项（State 一项明确拖后到大世界阶段）。对应 `examples/ecs/hierarchy.rs`；核心源码 `crates/bevy_ecs/src/hierarchy.rs`、`crates/bevy_ecs/src/relationship/`、`crates/bevy_transform/src/systems.rs`。

## 0. 直答：Bevy 也是用 Component 关联父子吗？

**是，而且和 Unity ECS（com.unity.entities）几乎同构**——都是"子实体上一个指向父的组件 + 父实体上一个装子实体列表的集合组件"：

| 概念 | Unity Entities | Bevy 0.19 |
|---|---|---|
| 子指向父 | `Parent : IComponentData { Entity }`（装在**子**上） | `ChildOf(Entity)`（装在**子**上，`hierarchy.rs:107`） |
| 父装子列表 | `Child : IBufferElementData`（动态 buffer，装在**父**上） | `Children(Vec<Entity>)`（装在**父**上，`hierarchy.rs:152`） |
| 本地变换 | `LocalTransform` | `Transform` |
| 世界变换 | `LocalToWorld` | `GlobalTransform` |
| 变换传播 | `LocalToParentSystem` 等（TransformSystemGroup） | `mark_dirty_trees → propagate_parent_transforms → sync_simple_transforms`（PostUpdate chain） |
| 单亲限制 | 有（Parent 单例） | 有（ChildOf 是普通组件，一实体一份） |

两个"同构之外"的差异：

1. **维护机制**：Unity ECS 两侧链接要手工维护（运行时加 `Parent` 后自己往父的 `Child` buffer append，或走 baking 生成）；**Bevy 由 ECS 组件 hook 自动维护双向一致**——你只写 `ChildOf` 一侧，`Children` 永远自动跟上。
2. **对照 GameObject 层**：Unity GameObject 的层级骑在原生 Transform 上（m_Father/m_Children，不是 ECS 数据）；Bevy 的层级是**纯 ECS 结构**，变换传播只是搭在它上面的普通 system 三件套，理论上你可以 disable TransformPlugin 只用层级不用传播。

命名警示：Unity 叫 `Parent`、旧版 Bevy 也叫 `Parent`，0.16 起改名 `ChildOf`——语义从"我是谁的孩子"读出来更直白（`doc(alias = "IsChild", alias = "Parent")` 还留着旧名）。方向没变：都是装在**子**身上。

## 1. 一对组件的分工

- **`ChildOf` = source of truth**（真身）：`pub struct ChildOf(#[entities] pub Entity)`。单值 Entity → 天然限制单亲、天然是树不是 DAG。想改父子关系，永远改它。
- **`Children` = 反向缓存**（RelationshipTarget）：`pub struct Children(Vec<Entity>)`，内层 Vec 是**私有的**（derive 强制，防止直接改导致失同步）。对外只暴露 `iter/len` + `Deref<[Entity]>`；`swap/sort_by` 是给渲染排序留的合法口子。
- 为什么两边都放：改子→父是"写"，一次 O(1)；查某实体的孩子若没有 Children 就得全库扫 `ChildOf`，有它就是 O(1) 取列表。用"写时多维护一份"换"读时零扫描"，和 Unity ECS 的 Child buffer 同一个动机。
- `linked_spawn` 属性：`Children` 声明为 `linked_spawn`，含义是 **despawn 父 = 连锁 despawn 全部子孙**；同时控制"linked cloning"（深拷贝整棵子树）。
- 一对一关系也支持：RelationshipTarget 里放单个 `Entity` 字段（如 `View(View)`），后 relate 的顶掉先 relate 的——同一套框架，不是层级专属。

## 2. 自动维护：hook 链路（机制核心）

声明即挂钩：`#[relationship(relationship_target = Children)]` / `#[relationship_target(relationship = ChildOf, linked_spawn)]` 由 derive 注册组件 hook（`relationship/mod.rs`）：

1. **insert `ChildOf(parent)` → `Relationship::on_insert`（mod.rs:150）立即跑**：
   - 自指检查：指向自己 → 警告 + 自动移除（有 `allow_self_referential` 属性可豁免）；
   - 目标不存在（已 despawn）→ 警告 + 自动移除 ChildOf——**失效自愈**，不会留悬空引用；
   - 一对一情形：把旧 source 的关系顶掉；
   - 然后给父实体的 `Children` 添加自己（没有就 `with_capacity` 建一个）。hook 本体同步跑在 insert 时刻，对父侧的修改是排队命令、同一次 flush 内落地（官方文档称之为 immediate）。
2. **移除/替换/despawn 子的 `ChildOf` → `Relationship::on_discard`（mod.rs:219，语义 on_drop）**：从旧父的 `Children` 摘除自己；摘空了 → 排队移除 `Children` 组件。执行时**再查一次**是否真的空——防"同帧摘掉又插回同一父"时误删（`on_discard` 里那个闭包注释明说了这个竞态）。
3. **`Children` 本身被移除/父 despawn → `RelationshipTarget::on_discard`（mod.rs:307）**：给每个 source 排队 remove 其 `ChildOf`——反向也自愈。
4. **父 despawn（LINKED_SPAWN）→ `RelationshipTarget::on_despawn`（mod.rs:332）**：给每个子排队 `try_despawn`，子的 despawn 又触发它自己的 on_despawn——命令队列**扩散式**递归（不是函数递归栈），整棵树连锁消亡。

要点：

- **你永远只操作 `ChildOf` 一侧**，`Children` 是只读投影。直接塞 `Children` 组件叫 `collection_mut_risky`（risky，官方明示别碰）。
- **逃生门 `RelationshipHookMode::Skip`** + `insert_with_relationship_hook_mode`：克隆、实体搬家时由框架手动接管集合、跳过 hook，防止重复登记（hierarchy.rs:1149 测试演示）。
- **这不是层级专属机制，是通用 Relationship 框架**：`Likes`/`LikedBy` 三行 derive 就能造自定义关系。ChildOf/Children 只是"正典实现"。框架还带类型擦除层（`RelationshipAccessor`，entity 字段偏移 + iter 函数指针），给 BRP/反射/BSN 动态访问用。
- 畸形层级（环、双亲）**运行期会在变换传播系统里 assert panic**（见 §5）——ECS 层的自愈只覆盖正常 insert/remove 路径，用 unsafe 强改才造得出坏树。

## 3. 写路径 API（EntityCommands / EntityWorldMut 双胞胎，hierarchy.rs:269-488）

| 意图 | API |
|---|---|
| spawn 时嵌套建树 | `.with_children(\|p\| p.spawn(...))`；声明式宏 `children![...]`（= `related!`，可作 bundle 的一部分一次成树） |
| 事后挂子 | `add_child` / `add_children` |
| 定位插入（保 sibling 顺序） | `insert_child(index)` / `insert_children(index)` |
| 摘链不杀 | `detach_child` / `detach_children` / `detach_all_children`（0.19 由 `remove_*` 更名——旧名容易被误读成 despawn） |
| 整批换血 | `replace_children` / `replace_children_with_difference`（性能口子：调用者自报"去/留/新增"三元组，debug 断言校验不变量，违规 panic） |
| 排序 | `Children::swap` / `sort_by`（只动显示顺序，不动结构） |

**despawn 语义**（最容易踩的坑）：

- `despawn(parent)` → **递归杀全部子孙**（LINKED_SPAWN）；
- `despawn(child)` → 只摘自己的链接，父的 Children 自动少一条；
- 想"杀父留子"：先 `detach_all_children()` 再 despawn。

**保世界坐标的重挂载**：`bevy_transform` 的 `TransformHelper::compute_global_transform` + `propagate_transforms_for<F>`（systems.rs:11）支持"同帧移动 + 同帧渲染"的按需全局变换计算——第三/四步取数时用得上。

## 4. 查询侧（relationship_query.rs）

- 直接组件路径：`Query<&ChildOf>` 拿父；`Query<&Children>` 拿子列表；`With<ChildOf>` 过滤"所有非根"。
- `Query` 扩展方法（泛型在关系类型上，不限 ChildOf）：`related<R>(e)`（取关系目标）、`relationship_sources<S>(e)`（取 source 列表）、`root_ancestor<R>(e)`（爬到根）、`iter_ancestors` / `iter_descendants` / `iter_descendants_depth_first`（`AncestorIter`/`DescendantIter`）、`iter_leaves`、`iter_siblings`。
- **全部假设树形**：关系带环会无限循环（文档反复警告）。
- 遍历拿 Entity 后仍要二次 `query.get_mut(child)` 取数据（见 hierarchy.rs 的 `rotate` system：`parents_query` 拿 `&Children`，再对每个 child 查 `transform_query`）——没有内建"子孙折叠查询"。

## 5. 变换传播：三系统链（层级存在的另一半意义）

`TransformPlugin` 在 **PostStartup 和 PostUpdate 各挂一遍**（首帧就对），chain 顺序 `plugins.rs:27-47`：

```
mark_dirty_trees → propagate_parent_transforms → sync_simple_transforms
```

1. **`mark_dirty_trees`（systems.rs:111）——静态场景优化的脏标记**：
   - 扫 `Changed<Transform> | Changed<ChildOf> | Added<GlobalTransform>` + `RemovedComponents<ChildOf>` 的实体，沿祖先链把 `TransformTreeChanged` 标记 set_changed；
   - 多线程版用共享**原子 bitset**（entity index → word/bit）做跨线程去重：爬到"已标记过的祖先"就提前停——同一脏子树不会被重复爬；
   - 源码注释自曝：`Changed<>` 是 table scan、慢，所以并行化了；
   - 受 `StaticTransformOptimizations` 资源控制（**默认 Enabled**）——这就是 bevy_city 第五步要用的 `StaticTransformOptimizations::Enabled`。
2. **`propagate_parent_transforms`——真正算 GlobalTransform**：
   - 从根（`Without<ChildOf>`）`par_iter_mut` 起步：`root.global = Transform`，然后对每个子递归 `GlobalTransform = parent_global.mul_transform(local)`；
   - `child_query.iter_many(children)` 沿 `Children` 走，**assert `child_of.parent() == 当前实体`**——双向校验，环/双亲直接 panic（安全注释里画了菱形反例）；
   - 并行版是 work-sharing 队列（chunk=512）：深度优先走，分支进 outbox 分给别的线程；根上只钻 1 层（防单线程独钻深树），worker 里 max_depth=10_000；
   - **`set_if_neq`**：算出来没变就不写——不污染 `Changed<GlobalTransform>`，代价是一次相等比较。第四步的变更检测直接受益：静态物体的 GlobalTransform 不会假变更。
3. **`sync_simple_transforms`**：不在层级里的散实体（`Without<ChildOf> + Without<Children>`）直接 `GlobalTransform::from(Transform)`；外加孤儿（`RemovedComponents<ChildOf>`）补算——摘掉父的当帧也要更新。

调度位置：PostUpdate。帧内顺序是 Update（改 Transform）→ PostUpdate（传播）→ 渲染取数读 GlobalTransform，同帧生效。

## 6. 周边效应

- **可见性传播**同走这棵树（`InheritedVisibility`，bevy_render 的 propagation systems）——hierarchy.rs 头注释明说 Transform 和 Visibility 都自动传播。
- **实体克隆**：`linked_cloning` + LINKED_SPAWN → 深拷贝整棵子树（EntityCloner）；BSN 场景 Template 实例化走的就是这条链路（呼应《BSN场景语法》§2.3）。
- **B0004 警告**：`TransformPlugin` 挂 `ValidateParentHasComponentPlugin<GlobalTransform>`（bevy_app/src/hierarchy.rs）——一个 `Insert` observer 检查"子有 GlobalTransform 而父没有"，发 message 延迟到 Last 再确认一次才 warn。**Observer + Message 两套机制在一个小插件里都用上了**（呼应《事件系统全景》）。这个警告的含义：传播查询假设父必有 GlobalTransform，父缺了子就无人传播。

## 7. 性能账与坑清单

- `Children` 增删 = Vec 中间插入/删除 O(子数) + 组件变更；频繁重排要批量走 `replace_children_with_difference`。
- **子树脏标记的开销是"每次变更向上爬到根"**：静态物体多、动得少的场景赚；全场都在动（典型游戏）反而亏——所以资源是可关的（`Disabled`）。大世界正是它设计的目标场景。
- 层级深度直接进传播成本：每层一次矩阵乘 + 一次查询取数。静态几何**拍平/合并网格**（第五步 `merge_car_meshes` 的动机之一）比维护深树便宜。
- 单亲 + 树形：DAG（一份数据多个父引用，Unity prefab 实例那种"链接"）不适用于 ChildOf——用资产句柄/Template，别用实体父子硬造。
- despawn 大树 = 命令队列扩散，量级 O(整棵树)，不崩但有 flush 成本。

## 8. 项目落点

- **第三步取数**：glTF 场景 spawn 出来就是 ChildOf 树，`Query<(&Mesh3d, &GlobalTransform)>` 直接读传播结果，**不要自己算层级乘法**；同帧 spawn + 渲染的特殊场景用 `TransformHelper`。
- **第四步变更检测**：`Changed<GlobalTransform>` 被 `set_if_neq` 保护——只有真变了才脏，拿它当上传触发器是可靠的；`Changed<ChildOf>` 也在 mark_dirty 的触发源里，重挂载会正确触发传播。
- **第五步**：`StaticTransformOptimizations::Enabled`（默认已开）就是"静态子树跳过传播"的开关；bevy_city 同款配置，验证时注意它假设"静态树真的不动"。
- **宿主壳（第二步）**：TransformPlugin 属于数据族不是渲染族，禁 RenderPlugin 后它照常跑——第三步之前的宿主壳里 GlobalTransform 语义就已经可用。

## 9. 延伸问答：为什么 Unity ECS 用 Dynamic Buffer，不像 Bevy 直接用 Vec？

**前提校准**：Bevy 的 `Vec<Entity>` 也不是"存在组件里"——组件槽位里只有 24 字节的 (ptr, len, cap)，实体数据本体在 Rust 堆上。所以差异不是"有无堆"，而是**堆归谁管、能否内联进 chunk、语言能否安全转移所有权**。

Unity 用 Dynamic Buffer 而不是 `List<T>`，是被三个硬约束逼的，不是不知道 Vec 好用：

1. **GC**：DOTS 的存在意义就是让热数据逃离托管堆和 GC 扫描。`List<T>` 是托管对象，把它塞进组件等于把 GC 拉回来，白干。
2. **Burst**：Burst 只能碰 unmanaged 内存，托管引用进不了 Burst 编译的 job。
3. **Job 安全系统**：跨 job 的别名检查靠声明式句柄（`BufferTypeHandle` + 只读/读写标记）在原生指针上做，托管集合没有这套挂点。

而"换个非托管 List"也不行，根源是 **C# 没有所有权和移动语义**：Rust 组件搬家（加组件 → archetype 变更）是对组件槽位的浅 memcpy + 所有权转移，Vec 的堆块原地不动、旧槽位自动失效，安全且便宜；C# 复制托管引用是共享别名，"move"这个词不存在，只能深拷贝或手工句柄管理。所以 Unity 选择把可变长数据做成"**chunk 内联 + 超出外溢到堆 + chunk 里只留指针头**"（`[InternalBufferCapacity]` 声明内联容量），把所有权问题推给 ECS 内存管理器统一收权。

这笔交易两边的账：

| | Unity DynamicBuffer | Bevy `Children(Vec<Entity>)` |
|---|---|---|
| 小 buffer 访问 | 内联在 16KB chunk 里，列遍历顺序访问，零指针追逐 | 每实体一次堆追逐；遍历一列 = 头连续 + 堆块散布 |
| 元素类型 | 必须 `unmanaged`，string/托管对象放不了 | 任意组件类型都行，灵活 |
| 容量语义 | 外露（内联容量、超容实体搬 chunk） | 不可见（Vec 自扩容） |
| job/多线程 | 句柄声明纪律 | Rust 借用检查兜底 |
| 失同步风险 | 读写要 `GetBuffer` 挂安全系统 | 抽象层禁止直接写，编译期挡住 |

**结论**：Bevy 的"Vec 好用"是 Rust 所有权送的——移动安全、无 GC、借用检查代替手工纪律；Unity 的"难用"是 C# 约束下保住 chunk 常驻性能模型的必要成本。两边都是在各自语言约束下的合理点，且 Bevy 侧同样留了后门：`RelationshipSourceCollection` 的后端有 `SmallVec`（内联！）、`EntityHashSet`、一对一 `Entity` 等，`Vec` 只是 `Children` 选的默认——说明"用 Vec"是封装出来的选择，不是架构必然；真遇到"一列 Children 遍历是瓶颈"的场景，Bevy 也能换成内联后端，做到 Unity 内联同款收益。
