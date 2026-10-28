# ash_renderer 九站阅读路线：边看边学边校验

本文登记 2026-10-27 定下的 ash_renderer 全库阅读顺序：沿一帧数据的生命周期分九站走，逐站按"讲意图与原理 + 随时提问质疑"的方式完成边看边学边校验。本文只管阅读顺序与各站校验要点，不主张任何代码改动；路上发现的问题按性质回填对应步骤或《待修复清单》，不在本路线内施工。

## 为什么是这个顺序

- **沿一帧数据走**：开机 → 地基 → 节拍 → 标准件 → 资产上场 → GPU 骨架 → 管线装备 → 场景账本 → 帧录制。一帧数据怎么流，阅读顺序就怎么排。
- **顺依赖方向**：common → render → scene → {overlay∥remote} → driver（步骤 7~8 质量复盘无环化后的分层）。顺着读，每站引用到的"前方内容"都还没读到的困惑最少。
- **覆盖学习主线三核心**：bindless（第 5 站）、GPU-driven（第 6 站）、大世界流送（第 8 站），正好对应《学习目标实现步骤》的主线。

## 九站表

"对象类别"列用的是第 1 站笔记里的五类划分（①设备根 ②帧级执行件 ③常驻数据表 ④渲染目标 ⑤管线），标出每站主讲哪类 Vulkan 资源对象；逐个对象的归属见《[第1站：入口与初始化链](第1站：入口与初始化链.md)》。

| 站 | 落点 | 规模（行） | 对象类别 | 主题与校验要点 |
|---|---|---|---|---|
| 1 | `main.rs` + `driver/host/init.rs` + `driver/host/mod.rs` | ~555 | 五类总览（创建方） | 入口与插件装配。看清渲染器以 Bevy 为宿主的插件拓扑；init 只当目录和依赖图读，不钻构造函数。量小热身。 |
| 2 | `render/core/device/context.rs` | 605 | ① Context | instance/device/queue 创建与 Vulkan 特性开关。全库一切 Vulkan 调用的地基；"支持/启用/使用三件事"纪律的主战场。 |
| 3 | `render/core/device/swapchain.rs` + `render/frame/frame_sync_settings.rs` | ~390 | ① Swapchain + ② | present 模式、帧槽、UNORM+SRGB view 三件套。看懂"一帧占一槽、fence 管执行序"的节拍——后面所有跨帧问题都挂在这个模型上。 |
| 4 | `render/core/record/barrier.rs` | ~100 | 不持有资源 | 图像布局屏障助手。体量最小杠杆最大：吃透"谁等谁、谁对谁可见"的屏障语义，后面读任何 pass 都能顺手校验屏障对不对。目前只有帧录制（`frame/frame_draw.rs`）在用它；uploader、Hi-Z、回读仍直接调 `cmd_pipeline_barrier`，compute 侧另有 `Handoff` 助手（`record/compute.rs`），读到时用同一套语义校验。 |
| 5 | `render/resident/`（descriptors、pool、instance_table、mesh_convert、uploader、light_table） | ~3910 | ③（+ ② Uploader） | **bindless 的心脏**，学习目标第一核心：GPU 常驻资源、描述符堆、实例表、上传票据。工作量最大，值得拆两次讲。 |
| 6 | `render/core/pipeline/compute.rs` + `render/core/record/compute.rs` + `render/passes/visibility/`（cull、hiz、occlusion、lod、indirect） | ~2380 | ⑤ compute（+ ④ HizPyramid） | **GPU-driven 核心**，学习目标第二核心。compute 骨架下沉为两半：`pipeline/compute.rs` 建管线，`record/compute.rs` 录 dispatch 与交接屏障，是总钥匙，先读它们再逐个环节。 |
| 7 | `render/core/` 的 binding/pipeline/record/target + `render/passes/`（gbuffer、shadow、lighting、transparent、post、overlay） | ~6720 | ④ + ⑤ 图形/全屏 | 先读 `core/` 里的四个标准件目录：`binding`（set 布局声明、shader ABI 表、管线布局配方与对账、storage 描述符集）、`pipeline`（图形句柄与全屏管线骨架）、`record`（绑定与 viewport、全屏 pass 录制）、`target`（颜色/深度目标），再按功能读 `passes/`：gbuffer（目标 + 写入管线）、shadow（级联矩阵 + shadow_build + 阴影深度图）、lighting（cluster + composite_pipeline）、transparent、post（bloom、present_tonemap_pipeline、smaa）、overlay。"装备库"性质，功能之间按想先看的效果挑顺序。 |
| 8 | `scene/content/` + `scene/mechanism/`（ledger、residency、collect_report 优先） | ~5730（优先三件 ~1810） | CPU 侧账本（驱动 ③ 增删） | Bevy 侧账本：一份资产两份真身（CPU 容器 + GPU 显存）、两层索引账（Ledger 行按实体粒度 / residency 按资产粒度）。大世界流送第三核心。 |
| 9 | `driver/host/frame.rs`（draw_frame）+ `frame_sync.rs` + `render/frame/`（frame_pool 帧槽、frame_draw 的 record_frame） | ~2750 | 五类全部（消费方） | 终点验收考场：质量复盘把 draw_frame 从 1065 行拆到 350 行、record_frame 从 1958 行拆到 1109 行（提交 bd48cb0a2 口径）。前八站读完后，这里的每一行都能对上前面的某个标准件——读它就是全库复盘。 |

行数为 2026-10-28 实测 `wc -l` 快照（main=fb74ff2cc），仅供量力安排，不作为长期口径。

## 边界

- **overlay/（egui 屏幕层）与 remote/（BRP 工具链）不进主线**：属于"边用边熟"，中途查证运行时行为时再进入。
- 本文只登记阅读顺序与校验要点，不主张代码改动；发现的问题回填对应步骤文档或《待修复清单》。
- 各站讲解中确认的结论沉淀进本文件夹的分站笔记；跨站通用、可独立传播的机制知识仍按入口篇 §九 另立知识点文档。

## 走法约定

- 每站先讲三样：这一站的意图、数据流、在整条链上的位置；再指出值得校验的点（屏障语义、生命周期、与提交说明的对应）。
- 边讲边问：随时打断提问与质疑，讲解人随时核对源码作答；结论分歧以源码为准。
- 一站讲完即校验完，再进下一站；中途换站允许（第 7 站装备库尤其适合按需跳读）。
