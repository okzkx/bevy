# 施工 3.3：贴图与 bindless 描述符

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。本段是 descriptor indexing 的核心实践。

**目的**：用已经验证的 shader 接口连接 VkImage、采样器和常驻描述符表，证明资源发布与访问安全。

**状态：🟩 四任务完成（2026-09-28：前置闸门关闭 + 3.3.1 贴图链路 + 3.3.2~3.3.4 描述符与槽位，证据见[3.3.2-3.3.4 施工记录](3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路.md)）。**边界：RenderDoc 工具验证未执行（无环境，见施工记录 §6）；acquire 屏障接线与 per-draw 消费归 3.4。

## 前置闸门

- [x] 3.2 的上传票据、跨族依赖和 staging 复用已验收，验证层与同步验证实际启用。（3.2 收官记录 + context.rs VkValidationFeaturesEXT 落点；本闸门探针同款常开，全程 VUID 收账）
- [x] 先用两张纹理加一个索引验证 naga 30.0.1 的 WGSL → SPIR-V：features 为 `wgsl-in` / `spv-out`；原生 binding_array 和 NonUniform、入口、capability 经过验证，不写 GLSL `nonuniformEXT`。（组 A/B 实测：NonUniform 自动装饰、GLCompute main、capability 集合全中；runtime 数组输出缺 `RuntimeDescriptorArray` capability（VUID-04680），**路线已定 runtime+capability 补丁器**，见施工记录 §4）
- [x] 冻结纹理数组、sampler 数组和每帧 UBO 的 set/binding 表。WGSL texture/sampler 分离，不能照抄 GLSL combined sampler 表。（表已冻结：set0 b0 纹理数组 / b1 采样器数组（UAB+PARTIALLY_BOUND 三层配套）、set1 b0 每帧 UBO、push 96B 偏移 0/64/68/80 双侧钉死；数组容量与 sampler 去重仍归 3.3.2/3.3.4 待决）
- [x] 最小 SPIR-V 经 `spirv-val` 和临时 Vulkan 管线试验通过后再定正式 layout；此试验不等于正式 3.4 场景绘制完成。（补丁后 runtime 与原生 fixed 双双过 val + 设备 compute 试验，采样读回全对、零告警）

## 任务清单

- [x] **3.3.1 贴图链路**：`Assets<Image>` → VkImage/view/sampler，核对格式与使用角色，base color 走 sRGB 解码、数据贴图走线性；读取 sampler 设置，静态 mip0 模式限制可采样 LOD。（2026-09-28 完成：`vulkan/images.rs` + 探针 `image_probe` 四组全绿 + 宿主实跑一帧合批"mesh 6 + 贴图 15 张（14.7MB，sRGB 5/线性 10）"零 VUID、WM_CLOSE exit 0；格式角色以 `texture_descriptor.format` 的 Srgb 后缀为准，`ImageSampler::Default` 按官方 ImagePlugin 默认 linear() 解析）
  - 上传沿用 3.2 票据，但补图像子资源范围、布局转换、跨族 release/acquire 与 shader-read 依赖。（release 随 transfer 提交（与迁出布局合成一条屏障），acquire 挂消费方——全链由组 C 实证并钉出配对语义：acquire 需以 release 的 oldLayout 为旧态重新声明同一转换）
  - `VERTEX_INPUT` 是 buffer 首个消费阶段的约定，不可作为所有图片屏障的固定模板；依赖按真实访问建立。（图像段独立录制：UNDEFINED→TRANSFER_DST→（拷贝）→SHADER_READ_ONLY，读回经 TRANSFER_SRC——VUID-01397 首跑实抓）
- [x] **3.3.2 特性与限额**：查询并显式启用 runtime array、实际需要的 nonuniform、sampled-image update-after-bind、partially-bound 等；`descriptorIndexing` 不代替具体特性。（2026-09-28 完成：context.rs 五位硬校验缺一出局 + 显式启用；D1 惯例 20 分位支持值全量落日志；UAB 轨五项限额对账通过容量 1024——实测素材：每阶段/每 set sampledImages 与 samplers 均 1048576、全池总额 4294967295；**钉号：UAB 限额在 properties2 扩展结构 `PhysicalDeviceDescriptorIndexingProperties`，不在普通 Limits**）
  - 初始容量以 1024 为候选，核对 UpdateAfterBind 的每阶段、set/layout、总池、sampler 限额后决定。（**已定案 1024**：双表 2048 + 2 帧 UBO = 2050 描述符，五项限额全满足；对账在 `BindlessTables::new` 内执行，不过即显式报错不截断）
  - 固定容量默认不需要 VARIABLE_DESCRIPTOR_COUNT；若启用，则配套 feature 与分配结构，且只能放最高 binding，不得让 texture/sampler 两个数组同时使用该旗标。（维持不启用；双数组并列的 set 结构事实上排除该路线）
- [x] **3.3.3 描述符对象**：set0 为常驻纹理/采样器表，set1 为每帧 UBO；需要 update-after-bind 的 binding/layout/pool 旗标配套，具体数量与 shader 一致。（2026-09-28 完成：新模块 `vulkan/descriptors.rs`——set0 双表（SAMPLED_IMAGE/SAMPLER ×1024，UAB+PARTIALLY_BOUND 经 BindingFlagsCreateInfo）+ set1 每帧 UBO×2（64B identity 初值）；三层配套在表内收齐后两层（binding 旗标 + layout `UPDATE_AFTER_BIND_POOL` + pool `UPDATE_AFTER_BIND`），feature 位第一层在 context.rs；stage flags 取 COMPUTE|VERTEX|FRAGMENT 超集；pipeline layout 归 3.4（表出布局/集合句柄）；数量与闸门冻结的 WGSL 接口一致，由探针组 C 生产表采样读回实证）
- [x] **3.3.4 槽位分配与发布**：free list、fallback 槽、上传票据与引用发布；draw 访问新资源前有完成依赖。（2026-09-28 完成：bump+回收栈 free list、fallback 白 1×1 占双表 0 号槽（采样器槽 0 的 linear 键与 fallback 共享——去重自然结果）、发布沿上传批次提交成功后执行且只写新槽（覆盖竞态结构上不存在）、票据进 `ImageCache` 账本行、GPU 侧等待归 3.4 接 draw；宿主实跑 15 张贴图 → 采样器槽收敛 2 种、零 VUID）
  - PARTIALLY_BOUND 只允许不被访问的空槽；缺资源用有效 fallback 或暂缓 draw。（fallback 槽 0 保证任何 draw 都有可采样有效槽；**首跑实证：访问未写入槽读回"碰巧全对"且验证层零告警——空槽 UB 不可依赖，槽位以 publish 返回值为准**）
  - 不覆盖 pending draw 仍可能读取的槽；旧槽、view、sampler 和资源本体等最后使用完成再回收。（M2 只写新槽；`retire_texture/retire_sampler` 释放入口带三条安全契约留档，运行时淘汰归步骤 4）
  - M2 可不实现运行时淘汰，但释放/复用接口必须有完成条件，完整动态回收在步骤 4 验收。
- [ ] **3.3.5 配套原理篇**：解释 flag 作用、容量与生命周期；区分支持/启用、补新槽/覆盖旧槽、上传完成/最后使用完成。

## 验证

1. 小 shader 的绑定、索引、纹理和 sampler 选择正确；CPU/SPIR-V layout 有可复查记录。
2. 验证层和同步验证实际启用，目标路径零 WARNING/ERROR；不得把缺验证环境的静默作为证据。
3. RenderDoc 可见贴图与各描述符；sRGB/线性、fallback、sampler 差异有最小例子。
4. 共享图片/采样器去重正确；分配、发布、延迟回收日志可对上票据，无在飞旧槽覆盖。

## 材料清单

施工结果尚未产生；当前依据为[施工计划 §1～§3](../施工计划：Bindless起步五段拆解.md)与本面板，完成后登记实际证据。

选型记录（2026-09-24）：[着色器源语言 WGSL，HLSL 为备用路线](选型记录：WGSL源语言与HLSL备用路线.md)——核查 naga 30.0.1 无 `hlsl-in` 后维持 WGSL/naga 选型；HLSL→DXC 仅作备用，切换须落账并重跑小样例。

前置机制篇（2026-09-24）：[显存机制：Buffer与Image之别、swizzle不透明与访问路径特化](显存机制：Buffer与Image之别、swizzle不透明与访问路径特化.md)——§0 判定线给出 3.3 三条实现约束：上传/读回必须走拷贝命令、image 分配独立于 MeshPool、layout 转换不能套 buffer barrier 模板。

前置闸门施工记录（2026-09-28）：[前置闸门施工记录：naga小样例与set-binding表冻结](前置闸门施工记录：naga小样例与set-binding表冻结.md)——四验收点全过；实证 naga 30.0.1 runtime 数组输出缺 capability（VUID-04680）、补丁器与定长双路线可跑；set/binding 表与 push 偏移冻结；**路线已定 runtime+补丁器（2026-09-28 用户拍板）**。

3.3.1 施工记录（2026-09-28）：[3.3.1-贴图链路：bevy Image到VkImage的批次化上传、布局链与跨族所有权](3.3.1-贴图链路：bevy Image到VkImage的批次化上传、布局链与跨族所有权.md)——`vulkan/images.rs`（image_spec/GpuImage/ImageCache）+ 上传票据图像段；探针四组（映射/上传闭环/跨族所有权/负例执法）+ 宿主实跑一帧合批零 VUID；新钉号 VUID-01397（读回不收 SHADER_READ_ONLY）、所有权屏障按对生效、wgpu-types 直接依赖。

机制篇（2026-09-28）：[纹理格式与Gamma：从png字节到采样值的色彩编码链](纹理格式与Gamma：从png字节到采样值的色彩编码链.md)——解码不改颜色值、用途决定格式（bevy_gltf 线性名单）、SRGB=同布局不同解释由 TMU 采样时解码；显存机制篇管"字节怎么排"，本篇管"字节怎么读"。

前置知识篇（2026-09-28）：[描述符索引特性族：能力位全景、三层配套与限额双轨](描述符索引特性族：能力位全景、三层配套与限额双轨.md)——3.3.2 施工依据：特性位≠关安全校验（能力类/契约类之辨，§0 判定线）、20 分位全景与启用清单（1 总开关 + 4 分位，§2）、UAB 三层配套（§3）、限额双轨（§4）、补丁器 capability↔feature 映射（§6）。

3.3.2-3.3.4 施工记录（2026-09-28）：[3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路](3.3.2-3.3.4-描述符与槽位：特性解禁、常驻描述符表与发布链路.md)——context.rs 五位启用+D1 全量落账；新模块 `vulkan/descriptors.rs`（限额对账/常驻双表/free list/fallback/发布）；build.rs 补丁器落地（定案兑现，naga 挪 build-dependencies，spirv-val 闸门+补丁+复验）；探针 `descriptor_probe` 三组全绿（发布/去重/耗尽负例/生产表采样/UAB 更新实证）零 VUID；宿主实跑三批发布 15 贴图→采样器槽 2 种、零 VUID、exit 0；**新钉号：SAMPLER-UAB 无需特性位已实测收账、空槽访问 UB 验证层不保证能抓、UAB 限额在 properties2 扩展结构、VUID-03252（特性集须覆盖全部消费者）、VUID-00312（无 FREE 位 pool 整体回收）**。

## 待决问题

- ~~naga 原生 WGSL 扩展输出到 raw Vulkan 的完整合法性：前置小样例决定，不凭依赖存在推断。~~（2026-09-28 已决：接口能表达能编译，但 runtime 数组输出缺 `RuntimeDescriptorArray` capability 被 VUID-04680 拒——capability 补丁后合法；定长数组原生合法。新待决如下）
- ~~**runtime+补丁 vs 定长数组路线**（3.3.1 开工前拍板）~~（2026-09-28 用户拍板：**runtime + capability 补丁器**——build.rs 内嵌补丁 + 补丁后重跑 spirv-val 兜底，定长保留为降级出口；build.rs 接线随首个正式 shader 落地）
- ~~固定数组容量与 sampler 去重策略：由所选设备实际限额和资产需求决定，不预先假定 1024 必然可用。（实测素材：本机 maxPerStageDescriptorSampledImages = 1048576、maxPushConstantsSize = 256B）~~（2026-09-28 已决：**容量 1024 定案**——UAB 轨五项限额实测全满足（见施工记录 §1）；**sampler 去重定案**——按 `SamplerKey` 功能参数键（滤波/mip/寻址/比较/边框色）分槽，色彩角色与被钉死的 lod/aniso 不进键，15 贴图 → 2 槽实测收敛）
- RenderDoc 描述符可见性验证：验证条款 3（贴图/描述符工具可见、sRGB/线性与 sampler 差异最小例）因本机无 RenderDoc 未执行——环境就绪后补做，不以零日志冒充证据。
- 输出端色彩编码（3.4 定）：swapchain 现为 B8G8R8A8_UNORM 线性直出；官方 bevy_render 优先 SRGB surface / 非 SRGB 也套 sRGB view。画出受光几何后缺编码会显形（中间调偏暗），届时在 swapchain SRGB 格式与着色器端编码之间拍板。见[纹理格式与Gamma篇 §6](纹理格式与Gamma：从png字节到采样值的色彩编码链.md)。
