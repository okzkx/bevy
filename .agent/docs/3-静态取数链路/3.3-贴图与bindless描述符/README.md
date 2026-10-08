# 施工 3.3：贴图 + bindless 描述符（descriptor indexing 全套）

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。**本段是步骤 3 的练习核心。**

**目的**：贴图上 GPU；descriptor indexing（bindless 起步形态）落地——set0 贴图大数组 + 槽位分配。

**状态：⬜ 未开工**

## 任务清单

- [ ] **3.3.1 贴图链路**：`Assets<Image>` → VkImage（bevy `TextureFormat` → VkFormat 映射；base_color 走 SRGB view 硬件线性化）+ 采样器（读 `Image::sampler` 设置）；进同一条 pending/timeline 链；
- [ ] **3.3.2 四件套 feature 全开**：`descriptorIndexing` + `runtimeDescriptorArray` + `shaderSampledImageArrayNonUniformIndexing` + `descriptorBindingSampledImageUpdateAfterBind` + `descriptorBindingPartiallyBound`（含 `descriptorBindingVariableDescriptorCount`，逐个查支持）；
- [ ] **3.3.3 描述符对象**：池（UPDATE_AFTER_BIND 旗标）/ 布局（set0 = 贴图数组 1024 槽，PARTIALLY_BOUND + VARIABLE_COUNT；set1 = 每帧在飞 UBO ×2）/ 集合分配——set0 一次分配终身用，set1 随帧轮转（寿命分家）；
- [ ] **3.3.4 槽位分配器**：free list 分配/回收槽位，上传完成即 `vkUpdateDescriptorSets` 补槽（update-after-bind：GPU 在跑也能写）；
- [ ] **3.3.5 配套《描述符原理篇》**：从 bind 模型到 update-after-bind，四件套逐 flag 回答什么问题（对计划 §1 表展开成深水区）。

## 验证（本段完成标准）

1. **验证层零告警**——本段最值钱的检查点（描述符误用几乎只有验证层能抓）；
2. RenderDoc 可见贴图数组与各槽位内容；
3. 贴图槽位分配/回收日志与上传数一致。

## 材料清单

- （施工进行中陆续增补）

## 待决问题

- [ ] **装 Vulkan SDK 补验证层**（步骤 2 遗留建议，本段兑现）：本机现裸奔，描述符误用告警很值钱。
- 槽位容量 1024 是否合理：按 `maxPerStageDescriptorSampledImages` 查询值夹紧，施工中定。
