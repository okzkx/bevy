# ash_renderer 九站阅读路线：边看边学边校验

本文登记 2026-10-27 定下的 ash_renderer 全库阅读顺序：沿一帧数据的生命周期分九站走，逐站按"讲意图与原理 + 随时提问质疑"的方式完成边看边学边校验。本文只管阅读顺序与各站校验要点，不主张任何代码改动；路上发现的问题按性质回填对应步骤或《待修复清单》，不在本路线内施工。

> 进度：第 1～4 站已读完（2026-10-30）；分站笔记有《[第1站：入口与初始化链](第1站：入口与初始化链.md)》与第 5 站上下两篇讲解正本（2026-10-30 写成，待逐段校验），第 2～4 站没有单独沉淀。下一站 = 第 5 站上半（5a）。第 5～9 站的落点按 O1、O2 及其下游六件施工后的源码（main=51884896a）重排过，细读顺序见下文"第 5～9 站细读指引"。

## 为什么是这个顺序

- **沿一帧数据走**：开机 → 地基 → 节拍 → 屏障 → 资产进显存并挂上 bindless → GPU 可见性骨架 → 管线装备 → 场景账本 → 帧录制。一帧数据怎么流，阅读顺序就怎么排。
- **顺依赖方向**：顶层是 common → render → scene → {overlay∥remote} → driver（步骤 7~8 质量复盘无环化后的分层）；render 内部是 core/device → core/binding（set 声明）→ resident → core/binding（管线配方）+ core/pipeline + core/record → passes → frame。顺着读，每站引用到的"前方内容"都还没读到的困惑最少。O2 之后 `resident/` 直接依赖 `core/binding/` 的 set 声明与写入器，所以绑定契约拆成两半，分别放在第 5、6 站开头，不再整体压到第 7 站。
- **覆盖学习主线三核心**：bindless（第 5 站）、GPU-driven（第 6 站）、大世界流送（第 8 站），正好对应《学习目标实现步骤》的主线。

## 九站表

"对象类别"列用的是第 1 站笔记里的五类划分（①设备根 ②帧级执行件 ③常驻数据表 ④渲染目标 ⑤管线），标出每站主讲哪类 Vulkan 资源对象；逐个对象的归属见《[第1站：入口与初始化链](第1站：入口与初始化链.md)》。

| 站 | 状态 | 落点 | 规模（行） | 对象类别 | 主题与校验要点 |
|---|---|---|---|---|---|
| 1 | ✅ 已读 | `main.rs` + `driver/host/init.rs` + `driver/host/mod.rs` | ~555 | 五类总览（创建方） | 入口与插件装配。看清渲染器以 Bevy 为宿主的插件拓扑；init 只当目录和依赖图读，不钻构造函数。量小热身。 |
| 2 | ✅ 已读 | `render/core/device/context.rs` | 605 | ① Context | instance/device/queue 创建与 Vulkan 特性开关。全库一切 Vulkan 调用的地基；"支持/启用/使用三件事"纪律的主战场。 |
| 3 | ✅ 已读 | `render/core/device/swapchain.rs` + `render/frame/frame_sync_settings.rs` | ~390 | ① Swapchain + ② | present 模式、帧槽、UNORM+SRGB view 三件套。看懂"一帧占一槽、fence 管执行序"的节拍——后面所有跨帧问题都挂在这个模型上。 |
| 4 | ✅ 已读 | `render/core/record/barrier.rs` | ~100 | 不持有资源 | 图像布局屏障助手。体量最小杠杆最大：吃透"谁等谁、谁对谁可见"的屏障语义，后面读任何 pass 都能顺手校验屏障对不对。目前帧录制（`frame/frame_draw.rs`）和全屏 pass 录制（`record/fullscreen.rs`）在用它；uploader、Hi-Z、回读仍直接调 `cmd_pipeline_barrier`，compute 侧另有 `Handoff` 助手（`record/compute.rs`），读到时用同一套语义校验。 |
| 5 | ⏭ 下一站 | 5a `core/device/memory/` + `resident/` 的 uploader、pool、mesh_convert、images；5b `core/binding/` 的 set_layout、descriptor_writer + `resident/` 的 slot_allocator、bindless_texture_table、frame_uniforms、instance_table、light_table | ~5320（5a ~2850 / 5b ~2460） | ③（+ ② Uploader） | **bindless 的心脏**，学习目标第一核心。上半讲数据怎么进显存（内存契约、上传票据、mesh 大池、贴图缓存），下半讲 shader 怎么找到它们（set 声明、按声明校验的写入、bindless 表、实例表、光源表）。天然分两次讲。 |
| 6 | 待读 | `core/binding/` 的 shader_abi、pipeline_layout + `build.rs` ABI 生成段 + `core/pipeline/compute.rs` + `core/record/compute.rs` + `passes/visibility/`（cull、hiz、occlusion、lod、indirect）+ `core/device/readback.rs` | ~2790 | ⑤ compute（+ ④ HizPyramid） | **GPU-driven 核心**，学习目标第二核心。先读"管线配方 → 与 shader ABI 对账 → 组装"这把总钥匙和 compute 骨架两半，再按录制顺序走可见性链。 |
| 7 | 待读 | `core/` 的 pipeline（graphics、fullscreen）、record（graphics、fullscreen）、target、`binding/storage_sets.rs` + `passes/`（gbuffer、shadow、lighting、transparent、post、overlay）+ 收尾 `binding/layout_registry.rs` | ~5340 | ④ + ⑤ 图形/全屏 | "装备库"：先读图形/全屏的标准件，再按功能读 pass，功能之间按想先看的效果挑顺序；最后用契约登记表把 22 条管线、13 份 set 声明对一遍总账。 |
| 8 | 待读 | `scene/content/` + `scene/mechanism/`（ledger、residency、collect_report 优先） | ~5770（优先三件 ~1810） | CPU 侧账本（驱动 ③ 增删） | Bevy 侧账本：一份资产两份真身（CPU 容器 + GPU 显存）、两层索引账（Ledger 行按实体粒度 / residency 按资产粒度）。大世界流送第三核心。 |
| 9 | 待读 | `driver/upload.rs` + `driver/host/drawlist.rs` + `driver/host/frame.rs`（draw_frame）+ `frame_sync.rs` + `render/frame/`（frame_pool、frame_draw 的 record_frame、render_mode） | ~4000 | 五类全部（消费方） | 终点验收考场：按 `Last` 调度的执行顺序读——上传编排 → DrawList 组装与环写入 → draw_frame → record_frame。前八站读完后，这里的每一行都能对上前面的某个标准件——读它就是全库复盘。 |

第 1～4 站行数为 2026-10-28 实测 `wc -l` 快照（main=fb74ff2cc）；第 5～9 站为 2026-10-30 重测（main=51884896a，`build.rs` 只计 ABI 生成段，第 7 站含 283 行契约测试）。仅供量力安排，不作为长期口径。

## 第 5～9 站细读指引

每站列文件读序（按依赖先后，前一个读完后一个就不会有"没见过的类型"）和值得校验的点；校验点优先挑能用第 3 站帧槽模型、第 4 站屏障语义当场对得上的地方。

### 第 5 站：常驻资源与 bindless

讲解正本：《[第5a站：bindless 数据怎么进显存](第5a站：bindless%20数据怎么进显存.md)》《[第5b站：shader 怎么找到它们](第5b站：shader%20怎么找到它们.md)》。

**5a 数据进显存。**

1. `core/device/memory/` 按 `role` → `contract` → `buffer` 读：`role.rs` 的 `BufferRole::spec` 是 8 种角色"usage + 必需属性 + 优先属性"的唯一一份表；`contract.rs` 的 `MemoryContract` 一次冻结设备内存事实，`find_type` 按规格选内存类型；`buffer.rs` 的 `GpuBuffer` 是全库 buffer 的唯一形态（恰量分配、持久映射、非 coherent 时自动 flush/invalidate）。本站所有表和池都只引用它们，不另立规则。
2. `resident/uploader.rs`：`UploadBatch` 合批、`Ticket`（单调递增的 u64 票据）、staging 两槽轮转（复用前等本槽上一张票据）。`Release`/`ImageRelease` 是队列族所有权让渡屏障，不是 staging 回收；生产路径按 CONCURRENT 共享，这两个列表恒空。
3. `resident/pool.rs` + `mesh_convert.rs`：`MeshPool` 的 bump 分配、`MeshSlot`/`PoolRange`、维护账与回收账（`MaintenanceLedger`、`RecycleLedger`）；`mesh_convert` 把 Bevy `Mesh` 转成池内顶点格式。
4. `resident/images.rs`：`ImageCache` 按资产身份驻留贴图，每张图一块 dedicated memory；`SamplerKey` 去重采样器。

校验要点：staging 释放挂在哪张票据上、谁保证 GPU 拷完才回收（对照第 3 站 fence 模型）；uploader 图像段的布局转换和 buffer 拷贝为什么分开录（对照第 4 站，uploader 是直接调 `cmd_pipeline_barrier` 的几处之一）；必需属性不满足时是报错冒泡，不是静默降级。

**5b 让 shader 找得到。**

1. `core/binding/set_layout.rs`：`SetLayoutDesc` 静态声明（binding 编号/种类/数量/阶段/旗标），`create` 先跑宿主自检再建原生布局；`merged_pool_sizes` 从声明推导 descriptor pool 容量；`DescSet` 是带声明身份的集。
2. `core/binding/descriptor_writer.rs`：每次写入按声明校验种类与元素界，`submit` 前查完备性——非 `PARTIALLY_BOUND` 的 binding 必须写满。
3. `resident/bindless_texture_table.rs`：`BINDLESS_TEXTURE_SET`（set0，UPDATE_AFTER_BIND + PARTIALLY_BOUND 的贴图/采样器表）、`BindlessTextureTable`（采样器按参数去重并计引用）、`SlotBinding`；`resident/slot_allocator.rs`：三张表共用的槽位分配器（立即归还 / 延迟回收两种）；`resident/frame_uniforms.rs`：`FRAME_SET`（set1，每帧槽一份 UBO）、`FrameUniforms`、`FrameUniformsData`。
4. `resident/instance_table.rs`：`INSTANCE_SET`（set2）与 `InstanceTables`；模块文档里的 hazard 纪律是第 9 站环写入的依据。
5. `resident/light_table.rs`：`LightTable`、`LightRecord`；为什么有两份见第 1 站笔记。

校验要点：bindless 槽位"按槽发布、从不写满"如何同时满足 `PARTIALLY_BOUND` 与写入器的完备性检查；UAB 只开在 sampled image / sampler 上的理由（对照第 2 站启用的特性）；set1 的帧槽 UBO 何时可写（第 3 站 `wait_for_slot` 之后）。可用 BRP 只读方法 `ash_renderer/layout_contract` 现场查看声明表。

### 第 6 站：GPU 驱动的可见性链

1. `build.rs` 的 `reflect_abi` 与 `assert_host_layout`：在 naga 校验之后，逐入口点列出实际用到的 `(set, binding, 描述符种类, 数组形状)` 和 push 字节数，生成 `OUT_DIR/shader_abi.rs`。
2. `core/binding/shader_abi.rs`：include 生成表，每个 shader 一个 `Shader` 静态项，spv 字节也挂在上面。
3. `core/binding/pipeline_layout.rs`：`PipelineDesc` 配方（shader、入口、按 set 编号排好的声明、push 范围、附件格式），`PipelineDesc::build` 统一完成对账 → 组装 → 建 module。
4. `core/pipeline/compute.rs` + `core/record/compute.rs`：建管线一半、录制一半；`Handoff` 是 dispatch 后的交接屏障，录制绑定前用 `assert_set_sequence` 按配方校验集的顺序。
5. `passes/visibility/` 按帧内录制顺序：`cull`（视锥剔除）→ `occlusion`（读 Hi-Z 做遮挡测试）→ `lod`（选档）→ `indirect`（生成间接绘制命令）；`hiz` 放最后，它在渲染写完深度后归约本槽金字塔，供两帧后同槽使用。
6. `core/device/readback.rs`：诊断用同步回读，独占一次提交并阻塞到 fence；只在 BRP 读数和 Hi-Z/透明列表诊断中用，不进帧循环。

校验要点：每个 `Handoff` 变体对应的"谁等谁、谁对谁可见"（第 4 站语义）；Hi-Z 同槽读取为什么不需要新同步——上一次写它的提交已被本槽 fence 证明完成（第 3 站模型）；对账口径里"布局是超集，shader 没用到的 binding 不算错"。

### 第 7 站：管线装备库

1. 图形/全屏标准件：`core/pipeline/graphics.rs`（`PipelineRef`）、`core/pipeline/fullscreen.rs`（全屏管线骨架）、`core/record/graphics.rs`（绑定、顶点/索引、viewport/scissor）、`core/record/fullscreen.rs`（全屏 pass 录制，入口出口用第 4 站的 `image_barrier`）、`core/target/`（color、depth 目标）。
2. `core/binding/storage_sets.rs`：只有 storage buffer 的"N 个 binding + 每帧槽一集"，光源分簇和透明层共用。
3. 按功能读 `passes/`：gbuffer（目标 + 写入管线）、shadow（`cascades` 级联矩阵 + `shadow_build` + `shadow_map` 深度图）、lighting（`cluster` 的 `LightClusters`/`ClusterBuildPass` + `composite_pipeline`）、`transparent.rs`（`TransparentTargets` + collect/sort/gather + 前向管线）、post（bloom、present_tonemap_pipeline、smaa）、overlay（egui 的 GPU 侧：图集、管线、UI 顶点环）。功能之间顺序自选。
4. 收尾：`core/binding/layout_registry.rs` 的 `ALL_PIPELINES` 与全部 set 声明，配合 `layout_contract_tests.rs` 看对账与负例。

校验要点：附件格式现在写在配方里，与渲染目标、swapchain 的 `SWAPCHAIN_COLOR_FORMAT` 是否一致；resize 时哪些目标和集重建（光源分簇随 resize 重建，透明命令链不重建）；cluster 与透明两套 set 唯一的交叉——`transparent_collect` 读光源分簇集 b0 的 `view_proj`。

### 第 8 站：场景账本

先读优先三件：`scene/mechanism/ledger.rs`（实体粒度的行账）→ `residency.rs`（资产粒度的驻留账）→ `scene/content/collect_report.rs`；其余 mechanism 文件按需展开，资产到货链可顺带看 `asset_events`、`asset_ops`、`asset_readiness`。

校验要点：一份资产的两份真身（CPU 容器 + GPU 显存）各由谁持有；行账的待盘点增量怎样交给第 9 站的上传编排；回收方向在哪里断开、谁负责。

### 第 9 站：帧录制（全库复盘）

按 `Last` 调度的执行顺序读：

1. `driver/upload.rs`：`flush_uploads` 消费账本的待盘点行，网格进池、贴图进 dedicated memory，合成一个批次提交；全程零裸 Vulkan 调用，只走第 5 站的出口。
2. `driver/host/drawlist.rs`：可绘行槽位枚举（`assemble_update`），以及模型环/材质环/在场环/LOD 变体表环的每帧写入（`write_rings`）。
3. `driver/host/frame.rs` 的 `draw_frame`（现约 350 行）：resize 闸门 → 等 fence → acquire → 组装 DrawList → 录制提交 → present。文件后半的 `cull_half`、`occlusion_half`、`lod_half`、`indirect_half`、`shadow_marks_half` 等助手各对应第 6、7 站的一个环节。
4. `render/frame/frame_pool.rs`（帧槽）→ `frame_draw.rs` 的 `record_frame`（现约 730 行）→ `render_mode.rs`；`frame_sync.rs` 顺带。

校验要点：环写入是否全部发生在 `wait_for_slot` 之后（第 5 站实例表的 hazard 纪律、第 3 站模型）；上传票据怎样变成图形提交等待的信号量；`FrameInput` 已到 16 个参数的上限（Bevy 系统参数硬约束），再加字段前要先合并分组。

## 边界

- **overlay/（egui 屏幕层）与 remote/（BRP 工具链）不进主线**：属于"边用边熟"，中途查证运行时行为时再进入。第 7 站读的 `passes/overlay/` 只是 egui 的 GPU 侧。
- 本文只登记阅读顺序与校验要点，不主张代码改动；发现的问题回填对应步骤文档或《待修复清单》。
- 各站讲解中确认的结论沉淀进本文件夹的分站笔记；跨站通用、可独立传播的机制知识仍按入口篇 §九 另立知识点文档。

## 走法约定

- 每站先讲三样：这一站的意图、数据流、在整条链上的位置；再指出值得校验的点（屏障语义、生命周期、与提交说明的对应）。
- 边讲边问：随时打断提问与质疑，讲解人随时核对源码作答；结论分歧以源码为准。
- 一站讲完即校验完，再进下一站；中途换站允许（第 7 站装备库尤其适合按需跳读）。
