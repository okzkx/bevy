# patch 池与分配回收流程：从相机位置到最终像素

> 9.5 功能流程导读（2026-11-24）。本段引入了一整套新的世界空间缓存与三条回收链，读代码前先看
> 这篇：三本账各是什么、一帧里谁先动谁后动、开关两种状态分别走到哪。逐任务的"做了什么"在
> [段 README](README.md) 的七篇记录里；本文只走数据流。

## 一、全景：一帧里的位置

```
draw_frame（Last）
  ├─ wait_for_slot → acquire → DrawList 组装 → 帧槽 UBO/环写入
  ├─ prepare_gi：Volume 跟随（各级整数体素偏移）→ 写 GI UBO → 清本槽计数 → 组装 GiFrameDraw
  └─ record_frame
       ① 进场
       ② compute 前置（IBL/求交/剔除/遮挡/LOD/indirect/shadow/cluster/透明）
       ③ shadow depth
       ④ G-buffer（不透明写四色 + 深度）
       ---- 四色 → SHADER_READ_ONLY、深度 → DEPTH_READ_ONLY ----
       ④.5 表面缓存（GI 开才录）：
            scrolling（偏移变化的帧）→ eviction → defrag × waves → patch_alloc
            → compute→片元屏障
       ⑤ 合成 + 透明（读 G-buffer 与 IBL；本段不读 patch 表）
       ⑥ Hi-Z 归约
       ⑦ Bloom → tonemap → SMAA edge/blend → LDR 停在可采样
       ⑧ patch 调试视图（视图关 = 零命令）：覆盖图画在 LDR 上 → 方片
          neighborhood（LDR + blend → swapchain，开渲染不关）→ UI
       ⑨ 出场 + 提交
  present
BRP（RemoteLast）：gi_set / gi_stats / gi_dump / gi_census
```

**为什么 ④.5 在 ④ 之后**：播种要 `textureLoad` 本帧深度与法线，② 段时 G-buffer 还没写；四色附件
在 ⑤ 前转只读（dst 含 COMPUTE）正好给 compute 采样。**为什么视图在 ⑧**：画在 tonemap 之后的
LDR 上，不经过 ACES，`frame_dump`/`gi_dump` 的同一条读回通道能取到。

## 二、三本账

| 账 | 位置 | 谁写 | 谁读 | 变不变 |
|---|---|---|---|---|
| **cell 双表** | `cell_patch`（cell→patch）/ `cell_marks`（同帧分配仲裁） | alloc 分配/刷新、scrolling/eviction 释放、defrag 改写指针 | 同左 + 视图查表 | 每帧局部变 |
| **patch 四表** | 几何 32B / 辐照度两份 64B / 统计 48B / 反向指针 4B | alloc 新建与刷新、defrag 搬移 | defrag/eviction/视图/9.6 | 每帧局部变 |
| **控制面** | `ring` 48B（flip/active/watermark + 两份 `{start,count,scan}`）/ `counters` 64B×2 / GI UBO 256B×2 | ring 与计数 GPU 原子改、宿主每帧写 UBO 并清计数 | 全部 compute + BRP 读数 | UBO 每帧整写 |

**名字与身份**：一条 patch 的身份 = `(级联, 世界体素, 方向桶)`；`cell_index = cascade·res³·16 +
16·(x·res²+y·res+z) + bucket`。patch 槽号（ring 位置）只是池里的地址，整理后可变；`patch_cell` 与
`cell_patch` 互指是"活"的唯一判据（搬走后旧槽的 `patch_cell` 不清，靠互指反查按死项忽略）。

**存储槽与相机无关**：`storage = (floor(p/v) + res/2) mod res`，级联偏移在代数上消掉；相机移动
不搬运数据，滚动只释放出界的槽。相机退远导致体素从细级跳到粗级时，是另一套 cell 与另一条 patch
（正常换代）。

## 三、新文件分工表（一层一句话）

| 文件 | 一句话 |
|---|---|
| `shaders/common/gi_cell.wgsl` | 格子系统全部纯函数 + 跨端记录结构 + 视图调色 |
| `shaders/scene/gi/patch_alloc.wgsl` | 1/4 屏幕播种：深度反投影 → 平面法线 → cell → 刷新或新分配 |
| `shaders/scene/gi/scrolling.wgsl` | 偏移变化帧：把出界体素的 16 桶 cell 三连释放 |
| `shaders/scene/gi/eviction.wgsl` | 心跳超 240 帧淘汰（bounce 分支已留、本段恒 0） |
| `shaders/scene/gi/defrag.wgsl` | 每波 512 条的原子取号压缩 + 读/写配置 flip-flop |
| `shaders/scene/gi/patch_view.wgsl` | 调试视图两形态：覆盖图 `fs_cover` + 方片 `vs/fs_marker` |
| `src/render/passes/gi/volume.rs` | 上述算术的 CPU 镜像 + 规格自检 + 单测 |
| `src/render/passes/gi/allocation.rs` | 播种 dispatch 的 push/组数与四集录制 |
| `src/render/passes/gi/view.rs` | 视图录制（LDR 转附件 → 覆盖图 → 方片 → 转回可采样） |
| `src/render/passes/gi/mod.rs` | `GiSettings`、六条管线配方与持有者、`GiFrameDraw` 录制体 |
| `src/render/resident/tables/surface_cache.rs` | 九类常驻表 + GI UBO + `GI_SET_DESC` + `GiSetLayout` |
| `src/driver/host/frame/gi.rs` | 帧侧编排：Volume 跟随、UBO/计数写、组装 ④.5 与视图束 |
| `src/remote/features/gi.rs` | BRP 四口（set/stats/dump/census） |
| `tools/gi_test.py` | 六条判定线一键驱动 |

## 四、逐时刻走读（GI 开、有相机的第 N 帧）

1. **帧首 `prepare_gi`**
   - 焦点 = 主相机世界位置；各级偏移 `offset_i = floor(focus / v_i + 0.5)`；与 `GiFrameLocal` 的
     上一帧偏移比较——任何一级变了，本帧 `scrolling = Some`。重开首帧强制一次滚动。
   - 写 GI UBO：`prev_view_proj`（上一帧矩阵，9.6 用）、`target_pos`、`voxel_min/res/cascades/
     capacity`、八槽偏移（未用级 0）、整理波数、法线来源、播种开关、帧号、淘汰门限 240、
     bounce 0、updateMax 32。随后 `clear_counters(slot)`（上一轮 GPU 已用完本槽）。
   - 组组装：`GiAllocDraw{四集, push, groups}`；视图束（若 `view > 0`）拿 LDR 视图/图像/尺寸。
2. **④.5 录制**
   - `scrolling`（可选）：`cascades·res³` 线程，把每槽换回世界位置，球外就释放该体素 16 桶。
   - `eviction`：`capacity` 线程，`(frame − heartbeat) & 0xFFFF > 240` 的活 patch 释放。
   - `defrag` × waves：每波 512 条，先读进私有存储、屏障后原子取号写回；扫到区间末尾翻转配置。
   - `patch_alloc`：1/4 像素轮转 + 子像素 16 帧轮转；已有 patch 刷新几何/心跳，新 cell 用帧号
     标签抢分配权、ring CAS 追加、写种子与统计。
   - 末尾 `compute→片元` 屏障。
3. **⑧ 视图**（关则跳过）
   - LDR 从 SHADER_READ_ONLY 转附件；覆盖图全屏写满（每个像素按自己的表面算 cell/patch）；
     方片 LOAD 叠加（人工深度测试 vs 场景深度）；转回可采样。
   - neighborhood 把 LDR + blend 画到 swapchain，UI 接在同一次渲染。
4. **BRP 时刻**
   - `gi_stats` 读本槽计数与 ring 头（两帧内新鲜）：活跃数 = `ring.active`，`ring_count` 含洞；
     `gi_ms` = GBuffer→SurfaceCache 的 GPU 段。
   - `gi_census` 先 `device_wait_idle`，抽检 ring 前 N 条：`cell_patch[mapped] == slot`、位置到
     所属体素中心 ≤ 体素、`direction_to_bucket(normal)` = 所属桶。

## 五、失败与稳态路径

- **GI 关（默认）**：`prepare_gi` 返回 None，④.5 整段不录、不录像屏障、不建视图；时间戳两个 mark
  相邻（`gi_ms ≈ 0`）；LDR 帧与 9.3 逐字节一致（`gi_test` ⑥）。
- **无相机**：不录（没有播种源与焦点）；`gi_stats` 仍可读表（冻结保留）。
- **空场（有相机无几何）**：播种线程全部在深度 0 返回，patch 0 条，不 panic。
- **池满**：ring CAS 失败 → `counters.failed++`、归还 marks 标签；不增长、不崩。
- **关后重开**：表不重建、ring 配置保留、池冻结保留；首帧强制滚动一次，随后按心跳自然淘汰旧项。
- **改规格**（只允许关态）：`device_wait_idle` → 用 `GiSetLayout` 的同一布局换 buffer/pool/sets →
  旧表 Drop 只拆 pool。布局不换，已建管线不悬空。
- **整理**：`active` 是"活 patch 数"（分配 +1、释放 −1），`ring_count` 是"含洞区间长度"（只增，
  整理完成后由写配置转正为存活数）。二者差值 = 待回收的洞。

## 六、与后续段的接口

- **9.6**：直接吃 patch 四表、`rank`（`statistics.state` 低 8 位）、heartbeat、`counters.failed`；
  bounce 分配 = "请求列表 + rank 1"两条 shader，不改本段结构（`bounce_enabled` 与 eviction 的降级
  分支已留）。种子重投影接 `prev_view_proj` 与 G-buffer 附件 2（motion vectors 本段未分配）。
- **9.7**：查表入口 = `gi_cell.wgsl` 的 `resolve_cascade → cell_index` 同一组函数；**法线来源必须与
  播种同源**（否则桶错位，缓存命中率凭空腰斩），只读 `rank == 0`；Volume 外回退 9.3 的
  GlobalProbe/SH-L2。
- **9.8**：显存、分配失败、池水位、cell 占用率、`gi_ms` 成组进压测；体素 warping、池容量、defrag
  频率的最终裁决在那里。

## 七、代码改动简要解析

两笔 feat（`git show --stat` 实数）：`87eb0cf32` 运行时 27 文件 +2863/−57；`5eedbe366` BRP 与
工具 7 文件 +979/−3（合计 32 文件 +3291/−60，不含文档）。

**新增组**

- `shaders/common/gi_cell.wgsl`（约 380 行）：全部格子算术 + 跨端记录结构 + 视图调色。取舍：
  纯函数、不声明绑定，播种/三件回收/视图四类入口共用同一份，避免"播种用 A 算法、查询用 B 算法"
  的桶错位。
- `shaders/scene/gi/*.wgsl` 五条入口（patch_alloc / scrolling / eviction / defrag / patch_view）。
  取舍：defrag 用原子取号而不是 workgroup 前缀和（naga/validator 限制）；视图与播放在同一模块里
  共用 `gi_cell`。
- `src/render/passes/gi/{volume,allocation,view,mod}.rs`：CPU 镜像 + 两束录制 + 六条管线配方与
  持有者。取舍：`GiSettings` 与 `GiFrameDraw` 都放 `mod.rs`，`frame` 层只消费；`volume.rs` 的
  单测是格子算术的唯一 CPU 参照。
- `src/render/resident/tables/surface_cache.rs`：九类表 + GI UBO + `GI_SET_DESC` + `GiSetLayout`。
  取舍：布局与表解耦（VUID 修复）；内存角色用 HOST_VISIBLE 直写省 staging。
- `src/driver/host/frame/gi.rs`：Volume 跟随与束组装；跨帧状态 `GiFrameLocal` 只存四样。
- `src/remote/features/gi.rs` + `tools/gi_test.py`：BRP 四口与六条判定线驱动。

**扩展组**

- `record.rs`：④.5 段与 ⑧ 段视图注入；`render_features.rs`：`gi: Option<GiFrameDraw>`。
- `post_chain.rs`：`record` 拆 `record_to_ldr` / `record_neighborhood`（命令序列不变）。
- `gbuffer_targets.rs` / `environment.rs`：采样集阶段位扩 COMPUTE（一份布局一份集）。
- `perf.rs` + `stopwatch.rs`：T5 SurfaceCache、`fetch_gi`、`gpu_gi_ms`。
- `init.rs` / `input.rs` / `frame.rs` / `layout_registry.rs` / `render/mod.rs`：资源链、帧编排与
  登记/导出；`frame_dump.rs` 抽公共 LDR 写盘助手；`brp.py` 四个子命令。
