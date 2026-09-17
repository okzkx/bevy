# 第一步：简单熟悉 Bevy 的结构（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 第一步，本文件夹存放该步的全部产出。

## 材料清单

- `Bevy结构笔记.md`——主产出：App/SubApp/Schedule/World 概念 + 帧循环时序图 + Unity 映射表（四问中 **Q1、Q3 已答**）
- `SubApp机制与取舍.md`——SubApp 专题（定位/并行真相/开销风险账）
- `Plugin与PluginGroup机制.md`——Plugin 专题（添加=build/生命周期/宏展开/disable 语义）
- `数据层全景：World、Entity、Resource与Asset.md`——数据层专题（2026-09-16 合并原《World与Resource》《Entity与Asset》去重：World 字段解剖/Resource=隐形实体+唯一性原理/Entity 位布局/Assets 双存储/tick/数据放哪判定/同构表+bindless 落点）
- `DefaultPlugins分类.md`——四问之 **Q2 已答**：渲染族禁 8 个、数据/渲染 crate 已解耦、PbrPlugin 优雅降级
- `BSN场景语法与Unity场景对比.md`——Q4 消费端半篇：BSN 语法/Template 机制/功能盘点 + Unity scene YAML 对比（glTF 加载链路另算）
- `System与调度DSL.md`——调度体系专题：add_systems 四跳链、ScheduleLabel/Interned、System 运行时对象、IntoScheduleConfigs 配置树、建图→编译→执行、Schedule 心智模型 + Unity DOTS 对照、并行规则
- `System参数与数据访问.md`——参数全家桶专题（2026-09-16，收编待深入①②）：0.19.1 参数全量目录、Query D/F 模板与 Single/Populated 验证跳过、Commands 同步点三时机与手动控制、Local 详解、多 Query 共存规则、高频问答
- `实体写路径：Bundle、Commands与Query改值.md`——写路径专题（2026-09-17）：Bundle=出生组件清单与一步落位 archetype、Commands API 分组与错误处理、独占系统立即写、Query `&mut` 值路径与变更检测、结构/值两路径对比与选型口诀
- `Bevy编辑器路线与代码热重载.md`——Bevy editor-last 立场与 BRP/.bsn 基建；Rust 热重载三堵墙与原生热重载光谱
- 另：《Bevy结构笔记》已增补 runner 真身表 + update() 内部三层；《SubApp机制与取舍》已增补 §7 三创建者与渲染线程交接；《开发工具与语法笔记》已增补 repr 内存布局条目（2026-09-14）、add_systems 泛型套路条目（2026-09-16）
- 运行记录与截图（按需，尚未产生）

## 待深入清单（遗漏记录，2026-09-14）

来源：原《World与Resource》末尾的基础概念盘点（该篇 2026-09-16 已并入《数据层全景》，盘点职能留在本清单）。按优先级排序，完成一项勾一项：

- [x] **System 与系统参数全家桶**（Query 过滤器、`ParamSet`、独占系统）——✅ 2026-09-16 完成：调度侧见《System与调度DSL.md》，参数目录/Single/ParamSet/独占/验证机制见《System参数与数据访问.md》
- [x] **Commands 与同步点**（何时生效、能否手动控制）——✅ 2026-09-16 完成，见《System参数与数据访问.md》§3（三时机/距离合并/两 executor/手动控制全家）
- [x] DefaultPlugins 三分类清单（宿主族/渲染族/无关）——即四问之 **Q2**，✅ 2026-09-14 完成，见《DefaultPlugins分类.md》（M1 实测验证遗留到第二步）
- [ ] Messages 双缓冲（`MessageReader`/`MessageWriter` 游标机制）——抄 bevy_city 加载流前要懂（`examples/ecs/message.rs`）
- [ ] Observer 与观察者传播（`examples/ecs/observers.rs`）——bevy_city 用它补组件
- [ ] 关系与层级（`ChildOf`/`Children` 自动维护，`examples/ecs/hierarchy.rs`）
- [ ] 资产系统全链路（AssetServer 异步、AssetEvent、热重载）——即四问之 **Q4**（✅ 消费端半篇 2026-09-14 完成，见《BSN场景语法与Unity场景对比.md》；剩 glTF→Mesh3d 加载链路）
- [ ] State 状态机 + `RunFixedMainLoop` 定点步——大世界阶段（按天分帧、加载屏）才用，可拖后

当前状态：进行中（Q1/Q2/Q3 已答；Q4 完成 BSN/场景消费侧，剩 glTF 链路；待深入①②已勾 2026-09-16，前三项勾完即达第一步完成标准，剩 Messages/Observer/关系层级/资产系统/State 五项）。
