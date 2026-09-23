# 施工 3.3：贴图与 bindless 描述符

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。本段是 descriptor indexing 的核心实践。

**目的**：用已经验证的 shader 接口连接 VkImage、采样器和常驻描述符表，证明资源发布与访问安全。

**状态：⬜ 未开工。**不因 feature 名称或布局草案已写入文档就视为工具链已通过。

## 前置闸门

- [ ] 3.2 的上传票据、跨族依赖和 staging 复用已验收，验证层与同步验证实际启用。
- [ ] 先用两张纹理加一个索引验证 naga 30.0.1 的 WGSL → SPIR-V：features 为 `wgsl-in` / `spv-out`；原生 binding_array 和 NonUniform、入口、capability 经过验证，不写 GLSL `nonuniformEXT`。
- [ ] 冻结纹理数组、sampler 数组和每帧 UBO 的 set/binding 表。WGSL texture/sampler 分离，不能照抄 GLSL combined sampler 表。
- [ ] 最小 SPIR-V 经 `spirv-val` 和临时 Vulkan 管线试验通过后再定正式 layout；此试验不等于正式 3.4 场景绘制完成。

## 任务清单

- [ ] **3.3.1 贴图链路**：`Assets<Image>` → VkImage/view/sampler，核对格式与使用角色，base color 走 sRGB 解码、数据贴图走线性；读取 sampler 设置，静态 mip0 模式限制可采样 LOD。
  - 上传沿用 3.2 票据，但补图像子资源范围、布局转换、跨族 release/acquire 与 shader-read 依赖。
  - `VERTEX_INPUT` 是 buffer 首个消费阶段的约定，不可作为所有图片屏障的固定模板；依赖按真实访问建立。
- [ ] **3.3.2 特性与限额**：查询并显式启用 runtime array、实际需要的 nonuniform、sampled-image update-after-bind、partially-bound 等；`descriptorIndexing` 不代替具体特性。
  - 初始容量以 1024 为候选，核对 UpdateAfterBind 的每阶段、set/layout、总池、sampler 限额后决定。
  - 固定容量默认不需要 VARIABLE_DESCRIPTOR_COUNT；若启用，则配套 feature 与分配结构，且只能放最高 binding，不得让 texture/sampler 两个数组同时使用该旗标。
- [ ] **3.3.3 描述符对象**：set0 为常驻纹理/采样器表，set1 为每帧 UBO；需要 update-after-bind 的 binding/layout/pool 旗标配套，具体数量与 shader 一致。
- [ ] **3.3.4 槽位分配与发布**：free list、fallback 槽、上传票据与引用发布；draw 访问新资源前有完成依赖。
  - PARTIALLY_BOUND 只允许不被访问的空槽；缺资源用有效 fallback 或暂缓 draw。
  - 不覆盖 pending draw 仍可能读取的槽；旧槽、view、sampler 和资源本体等最后使用完成再回收。
  - M2 可不实现运行时淘汰，但释放/复用接口必须有完成条件，完整动态回收在步骤 4 验收。
- [ ] **3.3.5 配套原理篇**：解释 flag 作用、容量与生命周期；区分支持/启用、补新槽/覆盖旧槽、上传完成/最后使用完成。

## 验证

1. 小 shader 的绑定、索引、纹理和 sampler 选择正确；CPU/SPIR-V layout 有可复查记录。
2. 验证层和同步验证实际启用，目标路径零 WARNING/ERROR；不得把缺验证环境的静默作为证据。
3. RenderDoc 可见贴图与各描述符；sRGB/线性、fallback、sampler 差异有最小例子。
4. 共享图片/采样器去重正确；分配、发布、延迟回收日志可对上票据，无在飞旧槽覆盖。

## 材料清单

施工结果尚未产生；当前依据为[施工计划 §1～§3](../施工计划：Bindless起步五段拆解.md)与本面板，完成后登记实际证据。

## 待决问题

- naga 原生 WGSL 扩展输出到 raw Vulkan 的完整合法性：前置小样例决定，不凭依赖存在推断。
- 固定数组容量与 sampler 去重策略：由所选设备实际限额和资产需求决定，不预先假定 1024 必然可用。
