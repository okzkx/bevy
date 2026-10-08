# 施工计划：Bindless 起步五段拆解

> 2026-09-22 建档，2026-09-23 校验修订。步骤 3 主计划，对应 [README](README.md)，源码基线为本仓库 Bevy 0.20.0-dev、ash 0.38。
> **五段顺序与“一次一段、理解后再走”不变。**本轮纠正技术假设、补上传生命周期与受控验收，不把规划写成已实现；已实现代码的缺陷统一见[《已实现缺陷与修复验收》](材料/已实现缺陷与修复验收.md)。

## §0 判定线

1. **完整几何可验证**：FlightHelmet 六个 primitive 在两侧统一的不透明调试模式下核对轮廓、变换和深度；原始镜片为 `BLEND`，M2 不实现完整透明，但也不静默丢弃它后声称几何完整。
2. **材质与光照分开比对**：以统一 base-color/unlit 条件检查贴图、UV、采样与颜色空间，再以受控方向光和环境光检查法线与光照方向。Lambert 不要求与官方完整 PBR、曝光和后处理像素一致。
3. **架构落点正确**：资产内容按 Handle 从 `Assets` 取；mesh/贴图去重驻留；合批 staging、transfer 提交、descriptor indexing；PostUpdate 采集、Last 同帧提交。
4. **生命周期可证明**：上传完成才可使用；staging/命令缓冲安全复用；目标资源和描述符槽按最后使用完成回收；跨队列同步、共享方式与 image layout 有明确契约。
5. **验证实际启用**：从 3.2 起使用 `VK_LAYER_KHRONOS_validation` 和同步验证，覆盖上传、绘制、resize、异常分支、退出；确认环境启用后，目标路径 WARNING/ERROR 清净。环境缺失不得用“零日志”打勾。
6. **等待口径明确**：正常上传不逐资源/逐批 `device_wait_idle` 或 `queue_wait_idle`；池容量维护、swapchain 重建、退出等待单列。按已完成票据复用资源所需的 fence/timeline 等待是正常同步，不冒充完全无等待。
7. **两 Tier 纪律延续**：可安全恢复才 warn 后跳过；无法维持有效状态时冒泡并优雅退出。非必要不 `panic!`，不能在已 reset fence 或已 acquire 后无补偿地直接返回。

**当前前置闸门**：现有 timeline 显式启用回归、fence 重置、present 信号量复用、退出等待时机尚未修复。先按缺陷文档关闭并复测，再叠加 3.2.2～3.2.4；本轮只修文档。

## §1 Bindless 入门：改变的是资源访问方式

传统逐材质换绑路径通常在 draw 间选择不同 descriptor set；bindless 将资源放进常驻描述符表，shader 用索引取资源，减少逐对象描述符装配与换绑。**传统绑定并非必然“一对象一套 set”，bindless 也不自动消除 CPU 的逐对象遍历和 draw。**M2 仍可直接绘制，GPU 常驻实例与 indirect 过渡明确归步骤 5。

本步目标形态：CPU 按管线布局绑定常驻贴图表和每帧 UBO；每个 draw 提供模型矩阵、材质颜色和纹理索引。未来 GPU-driven 再把逐 draw 参数迁入 GPU 可索引的数据表。

### 特性、对象旗标与索引语义

| 项目 | 本步含义 |
|---|---|
| `runtimeDescriptorArray` | shader 使用运行期长度资源数组时需要；固定长度实现与运行期数组分开验证 |
| `shaderSampledImageArrayNonUniformIndexing` | 同一调用组内出现非一致纹理索引时需要；“两个 draw 的索引不同”本身不等于 nonuniform |
| `descriptorBindingSampledImageUpdateAfterBind` | 为相关描述符类型允许 update-after-bind；仍受 pending 使用范围与生命周期约束 |
| `descriptorBindingPartiallyBound` | 允许动态不使用的数组元素未写入，不允许 shader 随意访问空槽 |
| `descriptorBindingVariableDescriptorCount` | 仅在分配时需要可变描述符数量时启用；固定容量 1024 不因使用 runtime array 就必须开启 |

`descriptorIndexing` 不是替代上述各位的万能总开关。查询并显式启用实际使用的具体特性，缺少必要能力时明确报错，不建传统 bound 回退渲染器。

update-after-bind 还需要 **binding 的 `UPDATE_AFTER_BIND`、layout 的 `UPDATE_AFTER_BIND_POOL`、pool 的 `UPDATE_AFTER_BIND`** 配套。容量必须核对相应 UpdateAfterBind 的每阶段、set/layout、总池以及 sampler 限额，不能只看普通 `maxPerStageDescriptorSampledImages`。

**“可补写”不是“可覆盖在飞引用”。**新且未被 pending 命令访问的槽可按规则写入；旧槽、image view、sampler 及资源本体必须等最后使用结束才可复用/销毁。缺图用有效 fallback 槽或暂缓 draw，不采样空槽。

### 着色语言先做最小接口验证

选择 WGSL，经 naga 30.0.1 生成 SPIR-V；Cargo features 是 **`wgsl-in`、`spv-out`**。`nonuniformEXT` 是 GLSL 语法，不写进 WGSL。naga 的原生 WGSL 扩展支持 `enable wgpu_binding_array;`；该版本 push-constant 地址空间为 `var<immediate>`，均须以锁定依赖和小样例验证，不当成浏览器 WGSL 通用能力。

在 3.3 布局定案前验证两张纹理加一个索引的最小 shader：

1. 纹理和 sampler 在 WGSL 中是分离资源，先确定 set/binding 表；本步优先纹理数组加 sampler 数组，并显式传各自索引，允许采样器去重。
2. 核对 SPIR-V 的数组类型、NonUniform、所需 capability、入口和字段偏移；naga 的 capability 允许集合不等于强制发出对应指令。
3. `spirv-val` 检查与最小 Vulkan 管线试验通过后，才把该接口用于资源布局。若原生扩展输出不满足 raw Vulkan，先定位生成/能力问题并修订工具方案，不堆到完整场景后排错。

这只是工具与接口的先行验证，不提前宣称正式 3.4 绘制完成。设备支持、编译成功、SPIR-V 合法、最终采样正确是四个不同验收点。

## §2 终态设计：同一帧准备和提交

**目标是同帧数据交接，GPU 执行仍异步。**当前已实现 PostUpdate 采集和 Last 清屏调度；Last 对快照的正式消费随本步后续任务接入。

| 时段 | 工作 | 输出/约束 |
|---|---|---|
| Update | 游戏逻辑、相机宽高比等更新 | 主世界本帧改动 |
| PostUpdate | 变换传播后采集 `CollectedScene` | 拓扑快照：Entity、mesh/material 柄、最终矩阵 |
| Last：准备 | 查驻留缓存、整理 pending 与 DrawList | 已驻留资源不因每帧快照而重复上传；未到货可跳过并重试 |
| Last：上传提交 | 合批拷贝、必要的 release/layout 操作、signal 上传票据 | timeline 表达批次完成，不自动转移队列族所有权 |
| Last：图形提交 | 等依赖、必要的 acquire、清屏与 draw、present | 等待发生在 GPU 执行依赖处，不要求 CPU 先阻塞等上传完成再提交 |
| 后续复用/退出 | 根据完成票据回收 staging 和旧资源 | 上传完成与最后图形使用完成分别追踪 |

[旧跨帧示意图](_assets/frame-collect-draw-relay.png)仅保留为历史资产：其“N 帧采集、N+1 帧绘制”及“CPU 等上传完成才提交”的表达均不作现行依据。本轮不重制图片；上表为施工权威时序。

### 关键决策

| 决策 | 本步选择与边界 |
|---|---|
| 平台与能力 | 保留 Vulkan 1.3 设备基线；使用 `Vulkan12Features` 声明 timeline/descriptor-indexing 特性不是兼容 1.2 设备，支持与启用分开 |
| 顶点布局 | 选择交错 `pos+normal+uv`，vertex-input stride 32B；这是我们的布局选择，不是 Vulkan 禁止 SoA；不可未经验证复用为 WGSL storage struct |
| 池策略 | 顶点/索引 DEVICE_LOCAL 大池 + bump 分配；驻留缓存按资产身份去重；静态首次收齐后按需求和余量分配 |
| 容量不足 | 不写越界、不静默截断；本步可走显式受控扩容维护并记录等待，频繁无停顿增长留步骤 4；扩容不冒充正常上传零 idle |
| 上传 | 合批 staging，一次 flush 对应一次 transfer 提交；目标是减少碎提交，独立队列的并行收益另测 |
| 跨族资源 | 本步优先 EXCLUSIVE + 成对 release/acquire，范围按实际 buffer 区间或 image 子资源；同族无 ownership 转移。若改为 CONCURRENT 必须明确创建队列族集合与取舍，不能只删屏障 |
| 完成与回收 | upload ticket 管 staging/transfer command buffer 复用；最后 graphics 使用完成管旧资源/槽回收；同队列也须满足内存依赖 |
| 描述符 | set0 常驻纹理/采样器表，set1 每帧在飞 UBO；各自寿命独立。固定容量起步，VARIABLE_COUNT 非必需，使用时只能放在最高 binding |
| per-draw 参数 | `model`、`base_color`、纹理与 sampler 索引；字段顺序/offset/range 按 Rust 与 SPIR-V 实际布局冻结，不按分量和估算；128B 为应检查的最低设备上限 |
| 相机与深度 | 沿用 Bevy reverse-Z 投影，D32_SFLOAT 查支持，深度 clear=0、GREATER 类比较；深度附件数量/同步按在飞使用设计，随 swapchain 尺寸重建 |
| 变换与颜色 | 约定矩阵乘序、法线逆转置、viewport Y 与 front face；输入 sRGB 解码、线性光照、输出只编码一次，按实际 swapchain format 决定 |
| 采集与代码边界 | PostUpdate 在 `TransformSystems::Propagate` 后采；`draw_frame` 住 Last；main 只组装，编排在 host，Vulkan 资源在 vulkan 子模块 |

例如原字段顺序 `mat4 model; u32 tex_index; vec4 base_color` 的自然偏移为 0/64/80，span 为 96B，不是 84B。增加 sampler 索引后须重新核对；`repr(C)` 或“低于 128B”都不能独立证明 CPU/shader 对齐。

## §3 五段拆解

### [施工 3.1：ECS 侧取数](3.1-ECS侧取数/README.md)

**状态：已完成，零 Vulkan 上传。**材质 hook、WorldAsset 进场、相机/灯光、PostUpdate 采集四项均已收官。FlightHelmet 实测 primitive 6、顶点 55392、索引 284166、材质 6、贴图槽 24 去重 15。

快照只带拓扑；每帧重建是 M2 的阶段实现。3.2 必须另按资产身份去重驻留，不能因快照每帧有同一个柄就重复上传。增量发现/上传改造归步骤 4。

### [施工 3.2：buffer 侧上传](3.2-buffer侧上传/README.md)

**目的**：将 `Assets<Mesh>` 数据正确送入 GPU 池，证明合批、同步和复用完整成立。

1. 保留 3.2.1 transfer 队列搜索和 1.3 基线；修复 timeline 显式启用回归，实际创建和 signal/wait 验证。
2. 3.2.2 资源件：内存类型、DEVICE_LOCAL 顶点/索引池、HOST_VISIBLE staging、对齐/flush、容量与销毁责任。
3. 3.2.3 网格转换：POSITION/NORMAL/UV_0、U16/U32 索引、范围和偏移，按资产缓存去重，缺字段和不支持拓扑明确处理。
4. 3.2.4 合批提交：transfer 命令池/命令缓冲、timeline 票据、跨族 release/acquire、图形等待、staging 与命令缓冲回收、失败时票据不虚增。

**验证**：字节数和内容与资产匹配；工具可见池内容；正常上传零逐资源/逐批 idle；同族/异族依赖均有可复核路径；实际验证层和同步验证通过。池增长维护单独统计。

### [施工 3.3：贴图与 bindless 描述符](3.3-贴图与bindless描述符/README.md)

**目的**：先冻结可工作的 shader/descriptor 接口，再接入图像、采样器和常驻槽位。

1. 前置小样例验证 WGSL/naga → SPIR-V 与分离纹理/采样器 binding，布局选择有证据。
2. `Assets<Image>` → VkImage/view/sampler：格式、sRGB/线性角色、mip0 拷贝、图像布局、跨族和 shader-read 依赖；与 buffer 共用上传票据，但不机械复用其屏障。
3. 查询并启用实际特性，核对 UpdateAfterBind 限额，配套 pool/layout/binding 旗标。
4. 槽位分配、fallback、上传票据和引用发布；不覆盖 pending draw 仍可能读取的槽。M2 可不做运行时淘汰，但必须明确释放入口的安全条件。
5. 原理篇解释每个 flag 与约束，不以“GPU 在跑也能写”代替完整语义。

**验证**：最小 shader 和管线试验通过；RenderDoc 可查贴图/描述符；无效/缺失贴图走 fallback 或暂缓 draw；创建、发布、复用行为符合完成票据。

### [施工 3.4：管线与绘制](3.4-管线与绘制/README.md)

**目的**：第一次在正式帧循环画出完整调试模型。

1. 正式 vertex/fragment WGSL、CPU/shader 布局、矩阵/法线与颜色空间约定。
2. dynamic rendering 图形管线、vertex input、viewport/scissor、reverse-Z；先禁背面剔除，验证绕向与双面材质后再选择开启。
3. 深度附件随尺寸重建；区分同一附件的生命周期与多个在飞提交的读写同步，不能只凭“归 Swapchain”判安全。
4. `record_clear_and_submit` 生长为 `record_frame`，DrawList 同帧消费；先关前置同步缺陷，不能为了保留旧函数结构而保留错误顺序。
5. M2 对所有六个 primitive 使用明确的不透明调试策略并记录材质覆盖；不静默丢掉镜片。

**验证**：统一调试几何完整，重叠物体深度正确，非恒等/非均匀缩放变换正常；resize、最小化、异常返回和退出不回退。

### [施工 3.5：光照与受控对照](3.5-光照与同屏对照/README.md)

**目的**：分项过 §0 判定线，整理可复跑证据。

1. 相机、方向光、`GlobalAmbientLight` 及可选相机 `AmbientLight` 覆盖写入每帧 UBO，复用前等对应帧使用完成。
2. 固定几何、不透明、base-color/unlit、方向光四类对照条件；相同相机和材质简化，记录官方 PBR/后处理未对齐项。
3. 采用独立官方对照进程或并排截图，不为验收把 wgpu 初始化引入自研进程。
4. 收官记录终态架构、坑、证据与已知边界，更新路线图/入口/任务面板。

**验证**：§0 全条有证据；静态内容不会每帧重复上传；剩余问题如实列出，不把未来任务标成已实现。

## §4 本步不做

- 对象/资产增量、热重载与频繁池增长：步骤 4；但初次异步上传的复用、所有权和销毁安全不能延期。
- 正式 alpha blend、完整 PBR、normal/metallic/roughness 全套着色：非 M2 判定条件；本步覆盖明确的不透明调试与 base color。
- mipmap 生成：M2 可用 mip0 并限制采样 LOD；基础 mip 链在步骤 5 大场景对照前补齐，mip 流送是步骤 6 的另一项。
- GPU 剔除、常驻实例参数表与 indirect：步骤 5；遮挡、GPU LOD 与流送：步骤 6。
- descriptor buffer：保留独立学习实验，不是 Vulkan 1.4 核心、不阻塞 GPU-driven 收官。
- 蒙皮/动画、通用 RenderGraph、mesh shader、编辑器：另立专题，不塞入当前主线。

## §5 节奏与状态纪律

- 一次施工一段：跑通和验证后讲解机制，理解确认后才进下一段。
- 编号不重排；前置检查放段内闸门，拆细现有任务用子号。
- 计划可以在段间修订；更改验收口径必须说明缘由，本次是将透明/PBR 简化与“完整几何”拆开验证，不是假装已有完整透明支持。
- 文档写“目标”和“现状”两列含义；缺陷关闭须有代码和复测证据。本轮没有源码改动，未实施项全部保留未完成状态。

## §6 依据与证据边界

- 材质入口：`bevy_pbr/src/gltf.rs` 的转换函数公开、官方 handler 不公开；现有 AshMaterialHook 使用转换函数，不是完全不依赖 bevy_pbr。
- 调度：`ash_renderer/src/scene/collect.rs` 在 PostUpdate/Propagate 后采集，`host.rs` 在 Last 提交；旧跨帧注释不覆盖实际调度。
- 相机：`crates/bevy_camera/src/projection.rs` 的 `PerspectiveProjection::get_clip_from_view` 使用无限反向透视；环境光全局资源是 `GlobalAmbientLight`，`AmbientLight` 是相机组件。
- 资产：`assets/models/FlightHelmet/FlightHelmet.gltf` 的 `LensesMat` 为 `BLEND`，`HoseMat` 为双面；完整调试验收必须明确处理两者。
- 编译：锁定 naga 30.0.1 的 Cargo features 为 `wgsl-in` / `spv-out`。本机未装 SDK 不表示只能用 naga；本项目选择该工具链，小样例的 raw Vulkan 合法性尚待施工验收。
- [Vulkan 1.3.289 Features](https://github.com/KhronosGroup/Vulkan-Docs/blob/v1.3.289/chapters/features.adoc)：设备支持与逻辑设备显式启用分开；`Vulkan12Features` 不是低版本兼容分支。
- [Vulkan 同步与所有权](https://github.khronos.org/Vulkan-Site/spec/latest/chapters/synchronization.html#synchronization-queue-transfers)、[描述符更新规则](https://github.khronos.org/Vulkan-Site/spec/latest/chapters/descriptorsets.html#descriptors-binding)、[WGSL 内存布局](https://www.w3.org/TR/WGSL/#memory-layouts)。
- [VK_EXT_descriptor_buffer 提案](https://github.khronos.org/Vulkan-Site/features/latest/features/proposals/VK_EXT_descriptor_buffer.html)：独立扩展，是否替换由能力和实测决定。

本轮是源码/规范校验与文档修订，没有运行新 shader、GPU 上传或性能测试；这些不能在后续记录中写成已通过。
