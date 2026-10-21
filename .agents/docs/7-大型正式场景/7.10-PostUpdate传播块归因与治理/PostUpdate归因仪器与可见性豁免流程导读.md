# PostUpdate 归因仪器与可见性豁免流程导读

> 7.10 段两件新机制写给下一个读代码的人：怎么把 PostUpdate 劈到每系统毫秒（仪器），以及为什么宿主里所有实体都没有 Visibility（治理）。归因实数与判定线见[施工记录](7.10.1-7.10.3-PostUpdate传播块归因与治理施工记录：trace归因仪器、可见性集群豁免与候选计数治理.md)。

## 1. 归因仪器（trace_chrome，零代码侵入）

```
构建：cargo build -p ash_renderer --features bevy/trace_chrome,bevy/trace
运行：TRACE_CHROME=<json 路径> ./target/debug/ash_renderer.exe ...
落盘：宿主优雅退出（WM_CLOSE）→ bevy_log 的 FlushGuard drop → chrome JSON
聚合：python .temp/7.10/aggregate.py <json> <稳态起点秒>
```

机制链：`bevy_ecs/trace` 在每个 SystemMeta 构建期发 `info_span!(parent: None, "system", name = 类型名)`（function_system.rs:51）→ tracing-chrome 逐事件写 B/E 行 → 聚合器按线程栈配对、按系统短名聚合。三个使用纪律：
1. **调度窗口配对按精确显示名**（`"name":"schedule:`）——`check schedule ticks` 辅助 span 会混入窗口计数（首版实踩，配出负时长才现形）。
2. **系统 span 是根 span**（parent: None）——无法按父链归调度，按系统名的调度归属读注册点（bevy_camera/visibility、bevy_animation 均在 PostUpdate；draw_frame 是 Last 归 7.11）。
3. **trace 开销可观**（zero_day 全程 1.7GB）——绝对毫秒注明带开销，量级与前后对照才是结论。

## 2. 可见性豁免（visibility_exempt.rs）

```
PostUpdate（每帧）:
  visibility_exempt 系统：
    Query<Entity, With<Visibility>> 逐实体
    → commands.remove::<{Visibility, InheritedVisibility, ViewVisibility}>()
  （Commands 调度末落地 → 本帧集群已跑完，移除自下帧生效 → 新实体多付一帧）
```

- **谁在消费被移除的东西**：没有。全仓零引用三件套；本渲染器的可见性归 GPU 剔除/LOD/遮挡自算（5.x 成果），`ViewVisibility` 是 bevy 渲染管线的产物，宿主禁渲染后整条链无人读。归因实数：集群 ~5.8ms/帧（zero_day 7160 实体）。
- **移除后**：集群查询不再命中（`With<Visibility>` 类过滤不匹配），PostUpdate 里它们退化为空查询走访；相机实体不豁外（frame.rs 只读 Projection/GlobalTransform，相机失明后连"视图"都不再命中）。
- **传播正确性**：`GlobalTransform` 传播与账本对账不依赖 Visibility——BRP set-transform 写后读回、树窗、Mesh3d 查询零回退。
- **谁会重新长回来**：glTF spawn 波 / BRP spawn 新实体带三件套（required components）→ 下一帧本系统捕获，幂等无累积；计量在 `VisibilityExemptStats`。

## 3. 分工表（本段新增/扩展）

| 文件 | 一层职责 |
|---|---|
| scene/mechanism/visibility_exempt.rs | 三件套移除系统 + 插件 + 计量（本段新文件） |
| scene/mechanism/ledger.rs（update_ledger） | scan_candidates 改账本行数（候选集 == 行集恒等，去掉每帧 O(N) count 走访） |
| scene/mod.rs、mechanism/mod.rs、main.rs | 插件再导出与装配 |
| .temp/7.10/aggregate.py（不进仓） | chrome JSON 流式聚合器，7.11 复用 |
