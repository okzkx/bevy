# 第一步：简单熟悉 Bevy 的结构（材料目录）

对应 [学习目标实现步骤.md](../学习目标实现步骤.md) 第一步，本文件夹存放该步的全部产出。

## 材料清单

- `Bevy结构笔记.md`——主产出：App/SubApp/Schedule/World 概念 + 帧循环时序图 + Unity 映射表（四问中 **Q1、Q3 已答**）
- `SubApp机制与取舍.md`——SubApp 专题（定位/并行真相/开销风险账）
- `Plugin与PluginGroup机制.md`——Plugin 专题（添加=build/生命周期/宏展开/disable 语义）
- `World与Resource.md`——数据层专题（字段解剖/Resource=隐形实体组件/tick/Commands 延迟）
- `DefaultPlugins分类.md`——四问之 **Q2 已答**：渲染族禁 8 个、数据/渲染 crate 已解耦、PbrPlugin 优雅降级
- `BSN场景语法与Unity场景对比.md`——Q4 消费端半篇：BSN 语法/Template 机制/功能盘点 + Unity scene YAML 对比（glTF 加载链路另算）
- `Entity与Asset.md`——数据层 ID 专题：两套 index+generation 体系、Handle 强弱、组件字段里的 Handle 是实体→资产单向桥
- `Bevy编辑器路线与代码热重载.md`——Bevy editor-last 立场与 BRP/.bsn 基建；Rust 热重载三堵墙与原生热重载光谱
- 另：《Bevy结构笔记》已增补 runner 真身表 + update() 内部三层；《SubApp机制与取舍》已增补 §7 三创建者与渲染线程交接；《开发工具与语法笔记》已增补 repr 内存布局条目（2026-09-14）
- 运行记录与截图（按需，尚未产生）

## 待深入清单（遗漏记录，2026-09-14）

来源：《World与Resource.md》末尾的基础概念盘点。按优先级排序，完成一项勾一项：

- [ ] **System 与系统参数全家桶**（Query 过滤器、`ParamSet`、独占系统）——最优先，第三步写渲染采集系统天天用
- [ ] **Commands 与同步点**（何时生效、能否手动控制）——`World与Resource.md` §4 已开头
- [x] DefaultPlugins 三分类清单（宿主族/渲染族/无关）——即四问之 **Q2**，✅ 2026-09-14 完成，见《DefaultPlugins分类.md》（M1 实测验证遗留到第二步）
- [ ] Messages 双缓冲（`MessageReader`/`MessageWriter` 游标机制）——抄 bevy_city 加载流前要懂（`examples/ecs/message.rs`）
- [ ] Observer 与观察者传播（`examples/ecs/observers.rs`）——bevy_city 用它补组件
- [ ] 关系与层级（`ChildOf`/`Children` 自动维护，`examples/ecs/hierarchy.rs`）
- [ ] 资产系统全链路（AssetServer 异步、AssetEvent、热重载）——即四问之 **Q4**（✅ 消费端半篇 2026-09-14 完成，见《BSN场景语法与Unity场景对比.md》；剩 glTF→Mesh3d 加载链路）
- [ ] State 状态机 + `RunFixedMainLoop` 定点步——大世界阶段（按天分帧、加载屏）才用，可拖后

当前状态：进行中（Q1/Q2/Q3 已答；Q4 完成 BSN/场景消费侧，剩 glTF 链路；剩余最高优先 = System 参数全家桶）。
