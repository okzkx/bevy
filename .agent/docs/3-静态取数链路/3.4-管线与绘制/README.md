# 施工 3.4：管线与绘制——从清屏长成画场景

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。

**目的**：正式帧循环首次绘制完整 FlightHelmet 调试几何，串起着色器、管线、深度与 DrawList。

**状态：⬜ 未开工。**3.3 的小接口试验不替代本段场景绘制验收。

## 前置闸门

- [ ] 既有帧同步缺陷已按[缺陷文档](../材料/已实现缺陷与修复验收.md)关闭，不继承“同步骨架永远不动”的旧承诺。
- [ ] 3.3 的 WGSL/SPIR-V、texture/sampler binding 与 descriptor layout 已用小例子验证。
- [ ] 明确当前相机使用 reverse-Z；深度测试、清值与投影成套，不混用正向 Z 的 LESS 模板。

## 任务清单

- [ ] **3.4.1 正式着色器与数据布局**：vertex 做坐标变换，fragment 按纹理/sampler 索引采样并做简化光照；启动时编译失败走 Tier②。
  - Rust 字段偏移、矩阵乘序、shader member offset、push constant range 有统一表；原 `mat4/u32/vec4` 自然布局为 96B，不按 84B 直接上传。
  - 法线使用正确的逆转置/归一化，验证非均匀缩放；vertex-input 32B 布局不默认等同 storage-buffer 布局。
  - 贴图 sRGB 解码、线性光照、输出编码只做一次，依据实际 swapchain format 决定。
- [ ] **3.4.2 图形管线**：dynamic rendering、颜色/深度附件、动态 viewport/scissor、正确 vertex input；reverse-Z 使用 clear depth=0 和 GREATER 类比较。
  - 明确 Vulkan viewport Y、front face 和顶点绕向；先关闭背面剔除，验证负尺度/双面材质后再开启相应变体。
- [ ] **3.4.3 深度附件**：D32_SFLOAT 查支持，随 swapchain 尺寸重建；数量按在飞访问设计，或显式同步复用，不把“生命周期归 Swapchain”当成无读写冲突证明。
- [ ] **3.4.4 `record_clear_and_submit` → `record_frame`**：接上传票据、必要的 acquire barrier、清屏、直接 draw 与提交；错误返回前保持 fence/信号量/资源状态可恢复，否则优雅退出。
- [ ] **3.4.5 DrawList 同帧消费**：PostUpdate 传播+采集，Last 准备并提交；拓扑快照不跨 CPU 帧，GPU 执行可异步。
  - M2 可逐 draw push constants，步骤 5 迁移常驻实例表与 indirect；不宣称此时已消除 CPU 每对象装配。
  - 原模型 `LensesMat` 为 BLEND、`HoseMat` 为双面；M2 用明确的不透明调试覆盖绘制六个 primitive，记录与生产材质的差别，不丢弃镜片后声称完整。

## 验证

1. 两侧同源不透明调试条件下几何完整；六个 primitive、轮廓、UV 与变换可核对。
2. 重叠几何的遮挡正确；近远深度、非恒等变换、非均匀缩放的法线正确，不只检查原始头盔静态姿态。
3. 颜色空间无重复 gamma；base-color/unlit 测试与光照测试分开。
4. 验证层/同步验证实际开启；resize、最小化、异常返回、退出回归通过。
5. main 仍只组装，编排住 host，录制住 Vulkan 模块；允许为正确性调整同步边界，不为旧结构承诺保留错误代码。

## 材料清单

施工结果尚未产生；当前任务依据为[主计划](../施工计划：Bindless起步五段拆解.md)。

## 待决问题

- 深度附件按帧槽还是图像索引分配：本段按实际提交访问确定，必须有同步证据。
- 正式 push constant 字段顺序与 sampler 索引布局：沿 3.3 接口冻结并检查大小/偏移，不沿用旧的 84B 估计。
