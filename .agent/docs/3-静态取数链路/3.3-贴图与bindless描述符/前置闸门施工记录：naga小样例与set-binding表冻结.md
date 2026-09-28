# 前置闸门施工记录：naga小样例与set-binding表冻结

日期：2026-09-28。性质：3.3 前置闸门施工落账，不构成 3.3.1~3.3.5 任何任务的验收证据。

对应[3.3 面板](README.md)前置闸门四项与[施工计划](../施工计划：Bindless起步五段拆解.md) §1"着色语言先做最小接口验证"。

实现落位：`ash_renderer/examples/bindless_probe.rs` + `ash_renderer/examples/shaders/bindless_probe_{runtime,fixed}.wgsl`，运行 `cargo run -p ash_renderer --example bindless_probe`。探针骨架沿 3.2 三探针惯例（自含最小链、验证层 + VkValidationFeaturesEXT 同步验证常开、VUID 收账）。

## 0. 判定线

**naga 30.0.1 的 WGSL → SPIR-V 能表达并编译全部目标接口（原生 binding_array、NonUniform、var<immediate> push constant），但其 runtime 数组输出不满足 raw Vulkan：spv 后端不发射 `OpCapability RuntimeDescriptorArray`，spirv-val 以 VUID-StandaloneSpirv-OpTypeRuntimeArray-04680 拒绝；capability 段最小确定性补丁（注入 RuntimeDescriptorArray + SampledImageArrayNonUniformIndexing 两枚 capability 与 SPV_EXT_descriptor_indexing 扩展）后即合法。定长数组（`binding_array<T, 4>`）输出原生合法，不需要该 capability。**

由此得出三个事实，3.3 施工直接引用：

1. **四验收点全过**（设备支持/编译成功/SPIR-V 合法/最终采样正确，两模块独立取证）：设备 RTX 2060（API 1.3.289）五位 descriptor-indexing 特性位全支持；naga 编译全链通过；补丁后 runtime 与原生 fixed 双双过 `spirv-val --target-env vulkan1.3`；compute 采样读回三条路径（push 一致索引选绿、每调用非一致索引 NEAREST 取整红、LINEAR 0.9/0.1 混色，实测 0.8984/0.1016 在 UNORM 量化与 ±0.01 容差内）全部正确。
2. **NonUniform 是自动装饰**：WGSL 源码零手写标记（也不存在 nonuniformEXT 语法），naga 的 uniformity 分析对非一致索引的绑定数组访问自动发 `OpDecorate NonUniform`（两模块各 4 处，覆盖纹理/采样器取数与存储 I/O 指针），并连带声明 `ShaderNonUniform` capability 与 `SPV_EXT_descriptor_indexing` 扩展。
3. **工具链缺口只在一处**：capability 发射面。补丁是纯结构性的（capability/extension 段插词），不改任何指令——组 D 负例证明验证层在 `vkCreateShaderModule` 时内嵌 spirv-val 执法同一条 VUID-04680，补丁后模块在特性未启用的设备上则被 VUID-VkShaderModuleCreateInfo-pCode-08740 收账（capability 声明 ↔ 特性启用的执法闭环，见 §3）。

## 1. 前置闸门四项勾选

- [x] **闸门①（3.2 基线）**：3.2 上传票据/跨族依赖/staging 复用已收官（面板与施工记录齐），验证层 + 同步验证常开（context.rs `VkValidationFeaturesEXT` 落点 + 本探针同款常开，全程 VUID 收账可查）。
- [x] **闸门②（naga 小样例）**：两张纹理 + 索引，naga 30.0.1 `wgsl-in`/`spv-out`，features 恰此两个（default=[]）；原生 `binding_array`（runtime 与定长双形态）、NonUniform 自动装饰、GLCompute main 入口、capability 集合全部扫描核对（§2/§3）。**遗留决定**：runtime 形态须经 capability 补丁（§4 路线决策），不构成"原生输出即合法"。
- [x] **闸门③（set/binding 表冻结）**：表已冻结（§5）。WGSL texture/sampler 分离实做（两个独立 binding，无 combined sampler）；字段偏移按 SPIR-V 实际 Offset 装饰与 Rust `offset_of!` 镜像互证后钉死。
- [x] **闸门④（spirv-val + 临时管线试验）**：双模块过 val 后，各自在真实设备上建 compute 管线 + UAB/PARTIALLY_BOUND 描述符（4 槽写 2 槽）+ 贴图上传/布局转换 + dispatch，采样读回全对、验证层零告警。此试验仅证明接口与设备路径，**不等于** 3.4 场景绘制完成。

## 2. 组 A~D 实测摘要

| 组 | 内容 | 结果 |
|---|---|---|
| A 编译与结构 | 双模块 wgsl-in → naga::valid（IMMEDIATES + TEXTURE_AND_SAMPLER_BINDING_ARRAY[_NON_UNIFORM_INDEXING]）→ spv-out；手写 SPIR-V 词流扫描 | runtime 694 字 / fixed 700 字，SPIR-V v1.0；capabilities [Shader, ShaderNonUniform]；extensions [SPV_KHR_storage_buffer_storage_class, SPV_EXT_descriptor_indexing]；runtime 版 OpTypeRuntimeArray ×3（纹理/采样器数组 + storage 输出）、fixed 版 ×1；NonUniform ×4；set/binding 对 (0,0)(0,1)(1,0)(2,0)(2,1) 全中 |
| A 字段偏移 | SPIR-V OpMemberDecorate Offset vs Rust 镜像 `offset_of!` | push 结构 [0, 64, 68, 80]、每帧 UBO [0]、试验索引结构 [0, 8]，两侧一致 |
| B spirv-val | `spirv-val --target-env vulkan1.3`（VULKAN_SDK 定位） | runtime 原生被拒（VUID-04680）→ 补丁后过；fixed 原生过 |
| C 管线试验 | 每候选独立设备；特性先查后开；2×2 贴图 staging→`vkCmdCopyBufferToImage`→UNDEFINED→TRANSFER_DST→SHADER_READ_ONLY 屏障对；set0 UAB+PARTIALLY_BOUND；dispatch + push constant | 设备五位全支持（maxPushConstantsSize 256B、maxPerStageDescriptorSampledImages 1048576——1024 容量素材归 3.3.2）；两候选采样读回全对；组内 VUID 零条 |
| D 负例诊断 | 关 runtimeDescriptorArray 特性的设备：①原生词流 ②补丁词流 | ①被 VUID-04680 收账（验证层在 vkCreateShaderModule 内嵌 spirv-val 执法；管线创建本身仍返回成功——"成功返回 ≠ 规范许可"实锤）②被 VUID-VkShaderModuleCreateInfo-pCode-08740 收账（capability 声明但特性未启用） |

组 C 顺带修复一个真缺陷并留档：UAB 的 binding 旗标（`UPDATE_AFTER_BIND`/`PARTIALLY_BOUND`）要求 set layout 自身带 `VK_DESCRIPTOR_SET_LAYOUT_CREATE_UPDATE_AFTER_BIND_POOL_BIT`（VUID-VkDescriptorSetLayoutCreateInfo-flags-03000）——首跑被验证层抓到缺这层，补上后零告警。旗标三层配套（特性位/pool 旗标/layout 旗标）在 3.3.3 施工时照此全套。

## 3. 负例钉号

- **VUID-StandaloneSpirv-OpTypeRuntimeArray-04680**：`OpVariable` 以 `OpTypeRuntimeArray` 实例化 UniformConstant 变量，须声明 `RuntimeDescriptorArray` capability。这就是 naga 30.0.1 输出缺口的规范锚点，spirv-val 与验证层（内嵌同款校验）双重执法。
- **VUID-VkShaderModuleCreateInfo-pCode-08740**：模块声明 capability 而对应特性未启用。capability 与 Vulkan12Features 特性位的绑定关系由它执法——"支持 ≠ 启用"在 shader 侧的对应条款。

## 4. 路线决策（2026-09-28 用户拍板：runtime + 补丁器）

**定案：路线 A——runtime 数组 + capability 补丁器。**要点：build.rs 内嵌与组 B 同款的确定性补丁（插 RuntimeDescriptorArray/SampledImageArrayNonUniformIndexing 两枚 capability + SPV_EXT_descriptor_indexing 扩展，capability 必须插在 extension 区段之前），补丁后强制重跑 spirv-val 兜底；定长数组保留为补丁器失效时的降级出口。build.rs 接线随 3.3 首个正式 shader 落地（3.3.3 描述符对象/3.4 管线绘制消费 SPIR-V 时），探针内的补丁函数为已实证蓝本。

runtime 与定长两条路线都已被本探针证明设备可跑、采样正确；差别只在工具链处理：

| 路线 | 形态 | 代价 | 收益 |
|---|---|---|---|
| A. runtime + 补丁 | `binding_array<T>`（无长度）+ build.rs 内确定性补丁（~60 行，插 capability/extension 词）+ 补丁后重跑 spirv-val 兜底 | build.rs 里多一个纯函数步骤 | 描述符容量与 shader 解耦（shader 不随容量改动重写）；语义与"常驻纹理表"设计对齐 |
| B. 定长数组 | `binding_array<T, N>`（N=容量，如 1024） | 零补丁；改容量 = 改 shader 常量重编（build.rs 本来就逐次重编） | 工具链最短路径；SPIR-V 原生合法无后处理 |

推荐 **A**：补丁是编译期确定性变换、有 spirv-val 与验证层双重复验，收益是容量演进不动 shader 源；B 可作为 A 补丁器出问题时的降级出口。**DXC 切换条件未命中**——naga 没有卡住（接口能表达、能编译、补丁后合法），缺的只是输出 capability，补丁器即"可行替代写法"（选型记录的触发条件原文）。

## 5. 冻结的 set/binding 表（闸门③产物）

| set | binding | 类型（WGSL ↔ SPIR-V） | 容量 | 旗标/布局 |
|---|---|---|---|---|
| 0 | 0 | `binding_array<texture_2d<f32>>` ↔ SAMPLED_IMAGE 数组 | **待决**（1024 候选，按 3.3.2 限额与资产需求定；试验用 4 槽写 2） | UPDATE_AFTER_BIND + PARTIALLY_BOUND；layout 旗标 UPDATE_AFTER_BIND_POOL；pool 旗标 UPDATE_AFTER_BIND |
| 0 | 1 | `binding_array<sampler>` ↔ SAMPLER 数组 | **待决**（去重后容量） | 同上 |
| 1 | 0 | `var<uniform> FrameUniforms{ view_proj: mat4x4f }` | 64B/帧 × 在飞组数 | 普通 UNIFORM_BUFFER |
| push | — | `var<immediate> PushParams{ model: mat4x4f, tex_index: u32, sampler_index: u32, base_color: vec4f }` | 96B（offset 0/64/68/80，SPIR-V 与 Rust 双侧钉死） | COMPUTE 阶段；96B ≤ 128B 规范最低线 ≤ 本机 256B |

- 偏移证据：SPIR-V `OpMemberDecorate Offset` 实测 [0, 64, 68, 80]；Rust 镜像用 16B 对齐的 `Mat4`/`Vec4` 钉住——裸 `[f32; 4]` 对齐 4 会把 base_color 落到 72，与 shader 侧不一致，"repr(C) 或低于 128B 不能独立证明对齐"（施工计划 §2）由此有了具体反例。
- 小样例的 set2（storage 索引/输出，(2,0)(2,1)）是试验 I/O 专用载体，**不进冻结表**；3.4 的 per-draw 采样索引进 push constant（表中已含）。
- sampler 数组允许去重（同参数 sampler 合并槽位），去重策略归 3.3.4。

## 6. 边界

- 本闸门不覆盖：3.3.1 贴图票据接线（探针内联 staging+copy，未走 3.2 Uploader）、正式容量与去重（3.3.2/3.3.4）、槽位分配与回收（3.3.4）、RenderDoc 可见性与 sampler 差异最小例（README 验证 3）、任何性能结论。
- 探针为自含最小链（无窗口、无 swapchain），与主工程设备创建是两套 instance/device；主工程正式启用 descriptor-indexing 特性位是 3.3.2 施工项，本探针的启用配方（`feature_set`）只作参照。
- 纹理为 2×2 UNORM 小样，sRGB 解码、mip、压缩格式均未触及（3.3.1 任务面）。
