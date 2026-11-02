# GPU 视锥剔除流程：从相机平面到标记环的各级账面

> 5.2 功能流程导读（收官定例：走读 + 文末代码改动解析）。面向下一个读代码的人：每帧"谁在视锥里"这个问题如何从 CPU 的相机矩阵出发、经 112B push 进入 compute、落到 HOST_VISIBLE 标记环，再被诊断通道读走。判定证据归[施工记录](5.2.1-5.2.5-GPU视锥剔除施工记录：compute管线、可见标记与验证通道.md)，立案取舍归[段施工计划](施工计划：GPU视锥剔除拆解.md)。
> **一句话说明**：CPU 每帧把相机 view_proj 提取成六平面塞进 compute push，compute 对对象表逐槽做"8 角点世界包围盒 vs 六平面"保守求交，往标记环写 0/1——本段只有诊断读者，5.3 indirect 才是真正消费它的下游。

## 0. 为什么是这个形状

5.1 之后，"画什么"的全部数据已在 GPU（对象表/模型环/材质环），但**画不画**仍由 CPU 全权：DrawList 枚举全部在场槽位，逐槽照画。5.2 把"画不画"的前半问（在不在视锥里）也搬上 GPU——判断依据（包围盒、模型）本来就在 GPU 上，只有判断基准（视锥）在 CPU 的相机里。所以每帧的跨界面只有一样东西：**六个平面**。它们小（96B）、每帧变（相机动）、全员共享（一次 dispatch 一个视锥）——三个性质都指向 push constants（瞬态命令流参数的本职），而不是又一根 host ring 或 UBO 扩容。

可见标记本身是**每帧全量重写的瞬态产物**：值只与"本帧相机 × 本帧场景"有关，没有跨帧语义，所以它住帧槽环（与模型环同一条 fence 纪律），不进账本、不进失效链、没有增量可言。

## 1. 新文件与新件分工表（一层一句话）

| 件 | 位置 | 一句话职责 |
|---|---|---|
| `cull.wgsl` | `shaders/` | compute 单入口 `cs_main`：读 set2 三表 + push 六平面，写 `marks[slot]` |
| `vulkan/cull.rs` | `src/vulkan/` | CullPass（compute 管线+layout）+ `frustum_planes`/`aabb_visible`（CPU 参照，库测试钉语义）+ `pack_cull_push` + `record`（录制）+ `CullSettings`/`CullDiagnostics`（开关与诊断账面） |
| 标记环（第四族） | `vulkan/instance_table.rs` | `mark_rings`：HOST_VISIBLE ×2 帧槽 + set2 扩 b3（COMPUTE-only read_write storage，描述符 init 一次写入） |
| cull 录制点 | `vulkan/frames.rs` | `FrameDraw.cull`（Option）+ `record_frame` 渲染进场前 dispatch + 5.3 预置屏障 |
| 剔除编排 | `driver/host/frame.rs` | draw_frame 步骤 4a-2：开关裁决 →（关）host 全可见填充 →（开）pack push + dispatch 输入 → 诊断读数落账面 |
| CullPass 创建/拆除 | `driver/host/init.rs` | init 链 GraphicsPipeline 之后建（借 set2 layout），teardown 反序拆 |
| 面板剔除段 | `overlay/windows/debug_window.rs` | checkbox（写 CullSettings）+ 读数行（只读 CullDiagnostics） |
| BRP 方法族 | `remote/cull.rs` + `tools/brp.py` | `cull-stats`（GPU vs CPU 逐实体对照）/ `cull-set`（开关） |

## 2. 逐时刻走读（稳态帧：相机动、场景静）

以一个普通帧 N（在飞 2 帧，当前帧槽 = N%2）为准：

```
Update（场景侧）
  │ 账本对账、资产事件……（4.x 不变，剔除零接触）
PostUpdate
  │ 传播 → update_ledger（剔除零接触——可见性不是账本事实，是每帧派生量）
Last
  ├ flush_uploads（上传链不变；标记环不在上传链上）
  └ draw_frame
      ① resize 闸门 → wait_for_slot
      │    ★ 本帧槽 fence 已过 ⇒ 标记环本帧槽（N-2 帧 compute 写的）可以安全读
      ② acquire
      ③ DrawList 槽位枚举（不读标记——红线：绘制路径对可见性无感知）
      ④ 帧 UBO 写（相机 view_proj 在此算出）
      ④a 模型环增量 + 材质环全量（5.1 不变）
      ④a-2 剔除半边（5.2 新增，全部在 fence 纪律内）：
      │   bound = used_object_slots()（bump bound：在场+停尸房在途全盖住）
      │   ├ 开关开：planes = frustum_planes(view_proj)   ← CPU 唯一的剔除侧计算
      │   │        cull = CullPass::draw(planes, bound)   ← 112B push 打包 + 组数
      │   ├ 开关关：host 向标记环本帧槽写 bound×4B 全 1（A/B 的 B 面，零 GPU 工作）
      │   └ 诊断：读标记环本帧槽 → 逐可绘行计数 → CullDiagnostics（只进计量）
      ⑤ record_frame：
      │   进场屏障（颜色/深度布局，不变）
      │   → if cull: bind compute 管线 → bind set2 → push 112B → dispatch(⌈bound/64⌉)
      │   → 全局内存屏障（SHADER_WRITE→SHADER_READ，5.3 消费形状预置）
      │   → dynamic rendering 清屏+逐 draw 画（零改动——本段不消费标记）
      └ present / advance
GPU（异步）
  compute：每 invocation 读 objects[slot]（局部 AABB）+ models[slot]（模型），
           8 角点变换收紧 → 六平面 p 顶点求交 → marks[slot] = 1/0
  下一帧的 draw_frame 4a-2 把它读走（诊断账面）……
  （5.3 起：indirect 组装 compute 读同一份标记——屏障就是给它预置的）
```

**对照要点**（判定线 1 的两侧）：CPU 参照 = `conservative_world_aabb`（5.1 的 8 角点重紧）+ `aabb_visible`（六平面 p 顶点测试），与 shader 逐位同语义；BRP `cull-stats` 在 handler 里用**当前**相机/账本现算 CPU 列，与标记环的 GPU 列逐实体对——所以测试惯例是改完相机/变换等 ≥3 帧（读数陈旧 ≤2 帧）再查。

## 3. 三条特殊路径

- **空场（bound=0）**：不 dispatch、不填充、读数整段跳过——`CullDiagnostics` 保持上次值（首帧前为默认零值）。
- **开关关**：host 全可见填充替代 dispatch。此时 `cull-stats` 会报"GPU=CPU+视锥外数量"的 mismatch——这不是 bug，是 B 面（关剔除）的语义本身；开关 on 恢复一致。
- **despawn 后的槽**：bump bound 含停尸房在途槽，compute 仍会按**旧记录**算它的标记——旧标记无人读（账本行已清，DrawList 不枚举），槽排空复用后新记录覆盖写。标记环从来没有"删行"语义，全量重写天然覆盖。

## 4. 同步与 hazard 面（为什么敢这样写）

- **host 读标记 vs GPU 写标记**：读发生在 `wait_for_slot` 之后——本帧槽上轮（N-2 帧）的 compute 写已被 fence 等完；GPU 本帧的写（record_frame 里的 dispatch）在 CPU 读数**之后**才被录制提交。coherent 可见性 + fence 并发序，与模型环的宿主写同一条链，方向相反（这里 GPU 写、host 读）。
- **compute 写 vs 下下次 compute 写**（同槽 WAW）：N+2 帧的 dispatch 录制以"wait_for_slot(N+2 槽) = N 帧 fence 已过"为前提——宿主序隔开了两次写，无 GPU 侧竞争。
- **dispatch vs 同命令缓冲的绘制**：屏障（判定线 3）先立形状；本段绘制不读标记，屏障暂无真实消费者——它是 5.3 的预置，不是本段的必要路径（诚实记录在 `cull.rs::record` 注释里）。
- **同步验证层的角色**：对 host 读写全盲（fence 是唯一执法），但对 device 侧 compute→graphics 依赖有效——首跑实撞的 VUID-07988/08600（组号错位）就是它抓的。

## 5. 代码改动简要解析（git show --stat 实数：11 改 3 增，+851/−31）

**新增**：
- `ash_renderer/shaders/cull.wgsl` —— compute 着色器本体；`@group(0)`（组号随 compute 管线布局，非图形布局的 set2 位次——首跑 VUID 订正钉在头注释里）。
- `ash_renderer/src/vulkan/cull.rs` —— 本段核心：管线 + 参照实现 + push 排布 + 录制 + 开关/诊断账面，五块合一处；`frustum_planes` 用 `to_cols_array` 手提数学行（列主序 a[i*4+j]）。
- `ash_renderer/src/remote/cull.rs` —— BRP 两方法（对照/开关），红线边界写在模块文档。

**扩展**（按数据流顺序）：
- `vulkan/context.rs`（+9）—— 选族过滤加 COMPUTE 硬条件 + 就绪日志钉明（闸门①）。
- `vulkan/instance_table.rs`（+74）—— 第四族标记环 + set2 b3 + `mark_ring` 访问器；模块文档 hazard 表加一行。
- `vulkan/frames.rs`（+16）—— `FrameDraw.cull` + `record_frame` 渲染前 dispatch/barrier（消费 cull.rs 自由函数，不持 CullPass 句柄）。
- `driver/host/frame.rs`（+65）—— 步骤 4a-2 全部编排（开关裁决/填充/打包/诊断）+ FrameInput 三字段。
- `driver/host/init.rs`（+17）、`driver/host/mod.rs`（+3）—— CullPass 建链/拆链 + 两个 init_resource。
- `vulkan/mod.rs`（+8）—— cull 模块导出面。
- `overlay/windows/debug_window.rs`（+24）、`overlay/framework/ui.rs`（+7）—— 面板剔除段。
- `remote/mod.rs`（+7）—— 方法注册。
- `tools/brp.py`（+15）—— `cull-stats`/`cull-set` 子命令。

**刻意不做的**：绘制路径零改动（DrawList/图形管线/debug_draw.wgsl 都没碰——标记不进绘制是本段判定线）；上传链零改动（标记环不经 staging）；帧 UBO 零扩容（平面走 push）。
