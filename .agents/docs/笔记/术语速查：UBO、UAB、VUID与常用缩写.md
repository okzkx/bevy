# 术语速查：UBO、UAB、VUID与常用缩写

> 2026-09-28 建档。出处：bindless 描述符设计讲解（3.3.3/3.3.4 收官后）——行文里成排的缩写没有展开，读施工记录、知识篇、代码注释遇到不认识的缩写先查这里。每个词条给全称、一句话机制和本项目落点；深机制看对应知识篇。场景类术语（WorldAsset/引擎 World/glTF scene）另见[《场景三层含义》](../3-静态取数链路/3.1-ECS侧取数/场景三层含义：glTF scene、WorldAsset与引擎World.md)，本文不重复。

## 0. 核心三件（本项目行文最常用）

### UBO = Uniform Buffer Object（统一缓冲对象）

**装什么的**——存"整个 draw 共用的小块数据"：相机 view-proj 矩阵、灯光参数这类。名字是从 OpenGL 时代沿用下来的口语词，Vulkan 规范里的正式身份就是 `UNIFORM_BUFFER` 这个 descriptor 类型。

**本项目落点**：set1 b0 的 64B 每帧 UBO（`FRAME_UBO_SIZE`，vulkan/descriptors.rs）——现在是 identity 初值，3.5 把相机矩阵写进去。走传统"先写后绑"，所以它**不需要** UAB。

### UAB = Update-After-Bind（绑定后更新）

**什么时候允许写的**——默认契约：描述符集一旦绑进在飞的 command buffer，内容就冻结，不许再改。UAB 放宽这条时序契约：配齐配套旗标后，允许集合在飞期间往空槽里写新描述符。

**配套是三件套**（缺一即被验证层拒）：设备特性位（context.rs，第一层）+ binding 旗标 `UPDATE_AFTER_BIND`（经 `DescriptorSetLayoutBindingFlagsCreateInfo` 扩展结构挂入）+ layout 旗标 `UPDATE_AFTER_BIND_POOL` + pool 旗标 `UPDATE_AFTER_BIND`。

**本项目落点**：set0 常驻双表全靠它——表在启动时建好，贴图槽随资产到货陆续填，将来在飞命令缓冲引用着它也合法。**代价是时序安全责任转给应用**：验证层对"在飞改写"不保证执法，防线上只剩槽位纪律（只写 free list 给出的从未在飞的新槽）。

**与 PARTIALLY_BOUND 的分工**（两个开关各管一件事，常被混为一谈）：UAB 管"已绑定的集还能不能写"；PARTIALLY_BOUND 管"未写入的槽允不允许存在"——语义是"未写入的槽允许存在、只是不许访问"（访问空槽是 UB）。set0 两个旗标都挂：槽位陆续填靠 UAB，1024 槽绝大部分永远空着靠 PARTIALLY_BOUND 兜着，而 fallback 槽 0 保证任何 draw 都有可采样的有效槽。

### VUID = Vulkan Unique ID（规范约束编号）

**执法条文的身份证号**——Vulkan 规范里每一条约束的唯一编号，验证层报错时用它指认违反了哪条。全称形如 `VUID-VkDescriptorSetLayoutCreateInfo-flags-03000`（结构/命令名-参数名-编号），行文里惯用尾号简称：VUID-03000。

**本项目钉号（被实测抓到或推得，收账见各施工记录）**：

| 简称 | 全称锚点 | 一句话 |
|---|---|---|
| VUID-03000 | VkDescriptorSetLayoutCreateInfo-flags | binding 带 UAB 旗标则 layout 必须带 `UPDATE_AFTER_BIND_POOL`（首跑实抓） |
| VUID-08740 | VkShaderModuleCreateInfo-pCode | 模块 capability 申报 ↔ 设备特性启用配对执法 |
| VUID-04680 | StandaloneSpirv-OpTypeRuntimeArray | runtime 数组要求 `RuntimeDescriptorArray` capability（naga 懒发射缺口被它拒） |
| VUID-03252 | VkDeviceCreateInfo-pProperties | 启用的特性集须覆盖**全部消费者**（探针漏 timeline 被抓） |
| VUID-00312 | VkDescriptorPoolCreateInfo-flags | pool 无 `FREE_DESCRIPTOR_SET` 位则整体回收，不逐 set free |
| VUID-01397 | VkCmdCopyImageToBuffer2-pRegions | 读回拷贝的源布局不收 `SHADER_READ_ONLY`，须先迁 `TRANSFER_SRC` |

**项目钉子**：驱动接受 ≠ 合法——VUID 只有验证层执法，多数驱动不查；"跑通了"不能当合规证明（教训见宪法与特性族篇）。

## 1. 着色语言与工具链

| 缩写 | 全称 | 一句话 |
|---|---|---|
| WGSL | WebGPU Shading Language | 我们的着色器源语言，喂给 naga；本项目选型与 HLSL 备用路线见[《选型记录》](../3-静态取数链路/3.3-贴图与bindless描述符/选型记录：WGSL源语言与HLSL备用路线.md) |
| SPIR-V | Standard Portable Intermediate Representation – Vulkan | GPU 驱动消费的二进制着色器中间码，naga 的输出 |
| HLSL / GLSL | High-Level Shading Language / OpenGL Shading Language | DirectX / OpenGL 家族的着色语言；frenderer 用 HLSL |
| SSBO | Shader Storage Buffer Object | `STORAGE_BUFFER` 描述符类型，可读写的大块着色器存储；探针的输出数组用它 |
| SDK | Software Development Kit | 本项目语境 = Vulkan SDK（含验证层与 spirv-val） |
| naga | （工具名，非缩写） | Rust 写的着色器翻译器：WGSL → SPIR-V，build.rs 编译链的核心 |

## 2. 图像与色彩

| 缩写 | 全称 | 一句话 |
|---|---|---|
| sRGB | standard RGB | 带 gamma 曲线的非线性编码；采样端 TMU 按格式声明自动解码，输出端 ROP 自动编码 |
| UNORM | unsigned normalized | 无符号整数线性归一到 [0.0, 1.0]，无色彩曲线；与 SRGB 同布局不同**解释** |
| RGBA8 | 红/绿/蓝/透明 各 8 位 | 头盔贴图全落在这两档（Rgba8Unorm[Srgb]） |
| PNG | Portable Network Graphics | 头盔 15 张贴图的磁盘格式 |
| glTF | GL Transmission Format | Khronos 的 3D 场景资产格式（"3D 界的 JPEG"），本项目的模型来源 |
| PBR | Physically Based Rendering | 基于物理的着色；官方 StandardMaterial 是它，我们 3.4 只做调试着色 |
| LOD | Level of Detail | 细节层级；贴图语境 = mip 级别，max_lod=0 = 钉死只采第 0 级 |
| UV | — | 贴图坐标轴名（U/V 两轴），采样寻址模式按轴配置 |

## 3. 硬件与系统

| 缩写 | 全称 | 一句话 |
|---|---|---|
| TMU | Texture Mapping Unit | GPU 上做采样/格式解码的固定功能单元；sRGB 解码发生在这里 |
| ROP | Render Output Unit | 管线末端把像素写进附件的固定功能单元；sRGB 输出编码发生在这里 |
| DWM | Desktop Window Manager | Windows 合成器，决定 swapchain 可选的 surface 格式 |
| GPU / CPU / API | — | 不解释 |

## 4. 引擎与合批（Unity 对照）

| 缩写 | 全称 | 一句话 |
|---|---|---|
| SRP | Scriptable Render Pipeline | Unity 的可编程渲染管线；SRP Batcher 是它的材质合批器 |
| ECS | Entity Component System | Bevy 的架构模型；我们的采集系统住 PostUpdate |
| BRP | Bevy Remote Protocol | Bevy 的远程查询协议（JSON-RPC/HTTP，端口 15702）；World 检查工具候选底座 |

Bindless 与 Unity 四刀对照的完整机制（SRP Batcher/bindless/Instancing/Indirect 各砍一刀）见[《bindless机制》](../3-静态取数链路/3.3-贴图与bindless描述符/bindless机制：从换绑到索引——frenderer锚点与Unity批处理对照.md)。

## 5. 工程内部编号（非行业缩写，常与上面混排）

| 记号 | 含义 |
|---|---|
| M1～M5 | 里程碑刻度（验证节点），不参与施工编号 |
| D1～D6 | 本项目缺陷文档里的缺陷序号（非 Vulkan 概念） |
| 3.1 / 3.2 / … | 层级累积施工编号：L1 步 `N` → L2 段 `N.M` → L3 任务 `N.M.K` |
| set0 / set1 | 描述符集编号：set0 = 常驻双表（UAB），set1 = 每帧 UBO（普通） |
| b0 / b1 | set 内 binding 编号；set0 = b0 纹理数组 / b1 采样器数组，set1 = b0 UBO |
| V1 | 缺陷文档的验证层四态口径：未安装 → 未运行 → 未复现 → 已验证无告警 |
| Tier① / Tier② | 两 Tier 错误处理：① warn 丢弃继续，② 冒泡 main 优雅退出 |
| feat / docs / chore | 提交信息前缀：功能 / 文档 / 杂务（中文 Conventional Commits） |

## 6. 最容易混的一对（钉子）

**UBO 是"装什么"（数据容器），UAB 是"什么时候允许写"（时序契约）。**set1 的 UBO 走"先写后绑"，不需要 UAB；set0 的槽位要"边飞边填"，才要 UAB 三件套。判定一句话：**看到 UBO 想数据放哪，看到 UAB 想谁在保证没改到在飞引用。**
