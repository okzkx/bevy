# 资产加载方案对照：frenderer、Addressables、UE与Bevy的异步加载拆解

> 4.3 段机制讲解篇（2026-10-12，用户问"frenderer 的异步加载也能做到不卡顿、完成后才显示，和 Bevy 比呢"与"Unity 是 Addressables、UE 是怎样的、Bevy 的方案重点讲，哪种最好"两轮问答沉淀）。对应[段 README](README.md) 与[施工记录](4.3.1-4.3.4-异步就绪施工记录：pending账面、失败取消与动态加载.md)；frenderer 源码 F:\okzkx\rust-frenderer 逐文件实读。

## 0. 判定线：一个资产方案 = 四件事 + 运营层

看任何资产加载方案就拆这五件，缺的那件就是使用者要自己写的：

1. **IO 抽象**——字节从哪来（磁盘/网络/进程外服务），换数据源要改几层。
2. **依赖编排**——依赖图谁排（手递手递归 vs 图作业整批），"依赖全齐"怎么广播。
3. **就绪语义**——什么时候"能用"；失败、取消、等待中的状态可不可观测。
4. **卸载策略**——引用计数还是 GC，按什么粒度回收。
5. **运营层**——分组打包、地址/目录管理、远程分发（大引擎才有，小方案故意不做）。

## 1. frenderer 异步加载的真实形状：后台搬运 + 大锁 + 阻塞交接

模型链（`remote/renderer/src/renderers/remote_model_renderer.rs:45`）与环境贴图链（`controllers/environment/mod.rs:125`）同一模式：

```
thread_spawn(move || {
    let bytes = client().load_file(...)?;      // 子线程：网络取数+解析（真异步半边）
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    lock_mutex!(main_mutex());                  // ← 全局大锁
    let model = Model::from_gltf(...)?;         // 锁内建 GPU 资源（逐 primitive wait_idle 在此）
});
```

- **线程模型**：每次加载手起一个裸 `std::thread`（`thread_spawn`，fire-and-forget + `warn_unwrap_or_default` 吞错）。
- **交接时序**：`lock_mutex!(main_mutex())` 全局互斥量 + 贴图链锁内显式 `device_wait_idle()` 再更新描述符、原子替换场景状态——交接瞬间全设备停。
- **完成才可见**：对象级 Option 占位——每帧 `record_cmds` 里 `handle_loaded()` 轮询 channel，结果到了才把 `Option<Model>` 从 None 换成 Some；加载完成前渲染状态槽里根本没有它。
- **没有的半边**：无等待/失败/取消账面（失败吞成一行 warn，请求方永远等不到）；无取消闸（加载期间对象被删，闭包照样把结果写回场景状态——错误复活风险）；无身份级共享去重（同文件加载两次=两份 GPU 资源，`Model::from_gltf` 各建各的）。

**"不卡顿"的真实语义**：加载期间帧循环不被 IO 阻塞（IO 在子线程，货真价实）；但交接那一刻是隐藏停顿点——子线程持全局锁做逐 primitive 建 buffer + wait_idle，帧循环碰主状态就得等锁，低频加载+小资源时感知不到。停顿不是被消灭，是被"一次换完"的粗粒度藏住了——与 A/B 审计"CPU 阻塞收口"结论同根：异步加载只是把阻塞搬进子线程、错开发生时刻。

**与 4.3 的分水岭**：两边"完成后才显示"同构（frenderer=占位符，我们=账本行占位+非驻留不画）；差异全在交接时序——frenderer 用「停下来换」买安全（大锁+wait_idle），4.3 用「账面+票据」让交接不停帧（pending 记账 O(1)、等待在 GPU 侧信号量、零 wait_idle 零大锁）；账面买到的是"等待多久、失败几个、取消几个、谁还在等"可见。

## 2. Unity Addressables：地址层解耦 + Bundle 粒度

核心=地址字符串 → Catalog（地址→Bundle+内偏移映射）→ Provider（按类型选解码器）→ `AsyncOperationHandle` + 引用计数。真正买到三样：①分组打包（Bundle 粒度下载/卸载，代价=跨 Bundle 依赖冗余，粒度调优是出名的坑）②label/目录批量寻址③远程 CDN 分发。执行层：主线程发起、IO 在工作线程、完成回调主线程 `Update` 派发——就绪语义与 frenderer 的 `handle_loaded` 轮询同构，只是官方化。

## 3. UE：cook → pak → 异步线程 → 依赖图

最重的整链，四件事全做且最厚：

- **IO**：cooked 资产进 pak（ZenStorage），统一异步 IO 层批量预读（IOBatch）。
- **依赖编排（招牌）**：`AsyncLoadingThread` 把"加载一个对象"变成依赖图整批作业——请求 `FSoftObjectPath`，加载线程沿 import 链把整棵依赖树挂进异步加载队列，**全部完成才广播完成回调**（`FStreamableManager::RequestAsyncLoad`）。与 Bevy `LoadedWithDependencies` 同一语义。
- **就绪语义**：`TSoftObjectPtr`（软引用）+ StreamableHandle（RAII 管生命周期）；失败走 CoreRedirects。
- **卸载**：GC 可达性标记清除——与 Unity 引用计数是最大分野。
- **运营层**：AssetRegistry（依赖缓存，加载前可查图）+ PrimaryAssetId 分组 + pak/patch。

## 4. Bevy（0.20-dev）：中间层最薄、五层解耦最干净

`crates/bevy_asset/src/` 把资产方案拆成五个可替换的独立层：

```
AssetServer（API 面：load::<T>(path) → Handle，句柄先行内容后到）
   ├─ io/：AssetReader trait（io/mod.rs:195）——IO 完全可插拔
   ├─ loader.rs：AssetLoader 按扩展名注册（解码层）
   ├─ 依赖编排：loader 内 load_context 挂依赖 → DAG → 全齐才发 LoadedWithDependencies
   ├─ processor/：AssetProcessor 离线导入管线（AssetMode::Processed，lib.rs:297/317，对标 UE cook）
   └─ Assets<T>：强引用计数容器 + AssetEvent 5 变体 + AssetLoadFailedEvent
```

- **加载任务**在 `IoTaskPool` 跑 async（server/mod.rs:427），同路径请求合并；完成后事件经帧节拍分发（3.1.2 接过的"占位 Handle、异步到货"）。
- **AssetReader 是 trait**——frenderer"另起一个服务"的架构在 Bevy 里不是另起炉灶，是实现一个 `AssetReader` 指向资产服务进程。frenderer 把五件事揉进 `load_texture`，Bevy 拆成五层。
- **AssetProcessor 对标 UE cook**：离线导入（meta+processed_assets），但粒度到"处理后的文件"为止，**没有 pak 式打包**。
- **热重载**：`file_watcher` feature（io/file/mod.rs:7）——本步骤 4.4 段范围。
- **故意没有**：Addressables 式运营层（无分组/label/远程分发/Catalog）、UE 的 pak 与依赖缓存索引——设计立场是把①~④做成最小内核、⑤交给生态（bevy_asset_loader 等在补）。

## 5. 光谱对照与评价

| | IO 抽象 | 依赖编排 | 就绪语义 | 卸载 | 运营层 |
|---|---|---|---|---|---|
| frenderer | RPC 服务器（可） | 手递手递归（无图） | 对象级 Option | 无 | 转换服务有 |
| Unity Addressables | Provider 插件 | 编辑器/运行时双轨 | AsyncOperationHandle | Bundle 粒度引用计数 | 最全（Catalog/CDN/label） |
| UE | 统一异步 IO+pak | **依赖图整批异步（最强）** | 软引用+StreamableHandle | GC 可达性 | 最厚（Registry/patch） |
| Bevy | **AssetReader trait（最干净）** | 任务图+DAG | Handle+事件族 | 引用计数（GPU 侧回收要自己做——4.4 的题） | 故意没有 |

按场景判：工业项目要远程分发和目录管理 → **Addressables 最省心**；大世界流送与深度可控 → **UE**；学习项目与小型产品 → **Bevy 的薄中间层最合适**——四件事全部看得见摸得着。本项目实际已经自己走了一遍 ③（就绪语义：3.1.2 占位 Handle + 4.3 readiness）和 ④（GPU 侧回收：4.4 要做），正是 Bevy 故意留白的层。

**收拢定式**：四个方案是同一条光谱上的四个位置——frenderer 把全部四件揉进一个服务进程，Unity/UE 把五件全做厚做成产品，Bevy 把前四件拆成五个接口然后停手。评价资产方案别问"哪个好"，拿"四件事+运营层"去拆，缺的那件就是你要自己写的——frenderer 里写过的、与 4.3 里补的，恰好是同一件。
