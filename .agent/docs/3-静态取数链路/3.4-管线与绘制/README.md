# 施工 3.4：管线与绘制——从清屏长成画场景

对应[施工计划](../施工计划：Bindless起步五段拆解.md)。

**目的**：正式帧循环首次绘制完整 FlightHelmet 调试几何，串起着色器、管线、深度与 DrawList。

**状态：🟩 五任务完成（2026-09-29），证据见[施工记录](3.4.1-3.4.5-管线与绘制施工记录：正式着色器、图形管线与DrawList同帧消费.md)。**边界：RenderDoc 工具验证未执行（承 3.3，无环境）；光照是调试 Lambert，分项对照归 3.5；blend 关闭、运行时槽位淘汰归步骤 4。

## 前置闸门

- [x] 既有帧同步缺陷已按[缺陷文档](../材料/已实现缺陷与修复验收.md)关闭，不继承"同步骨架永远不动"的旧承诺。（3.2 前置闸门 V1 轮已关闭 D1~D6；本段不继承任何旧结构承诺——record_clear_and_submit 直接生长为 record_frame）
- [x] 3.3 的 WGSL/SPIR-V、texture/sampler binding 与 descriptor layout 已用小例子验证。（3.3 前置闸门四项已关；本段正式 shader 逐字段沿用冻结接口，build.rs 补丁流程全绿）
- [x] 明确当前相机使用 reverse-Z；深度测试、清值与投影成套，不混用正向 Z 的 LESS 模板。（bevy 投影 = 无限反向 Z（get_clip_from_view）；管线 GREATER + 清 0 成套，见施工记录 §3）

## 任务清单

- [x] **3.4.1 正式着色器与数据布局**：vertex 做坐标变换，fragment 按纹理/sampler 索引采样并做简化光照；启动时编译失败走 Tier②。
  - Rust 字段偏移、矩阵乘序、shader member offset、push constant range 有统一表；原 `mat4/u32/vec4` 自然布局为 96B，不按 84B 直接上传。（`pipeline.rs::PushLayout` offset_of! 静态断言 + WGSL + layout 三方互证 0/64/68/80；pack_push 手工排字节）
  - 法线使用正确的逆转置/归一化，验证非均匀缩放；vertex-input 32B 布局不默认等同 storage-buffer 布局。（cofactor 矩阵 ∝ 逆转置，det 在 normalize 中消失；(1.6,0.7,1.2)+rotY(0.6rad) 验证通过，法线光照正常）
  - 贴图 sRGB 解码、线性光照、输出编码只做一次，依据实际 swapchain format 决定。（采样端解码 → 全程线性 → ROP 经 SRGB view 编码，见待决翻面）
- [x] **3.4.2 图形管线**：dynamic rendering、颜色/深度附件、动态 viewport/scissor、正确 vertex input；reverse-Z 使用 clear depth=0 和 GREATER 类比较。
  - 明确 Vulkan viewport Y、front face 和顶点绕向；先关闭背面剔除，验证负尺度/双面材质后再开启相应变体。（**朝向定案：正高度 viewport 直出即与官方同向，负高度反而颠倒**——E5 光栅级实验 + 官方 color_grading 对照截图实证，证据链见施工记录 §3.3，"教科书式翻转"直觉不成立；cull NONE 为 M2 材质策略）
- [x] **3.4.3 深度附件**：D32_SFLOAT 查支持，随 swapchain 尺寸重建；数量按在飞访问设计，或显式同步复用，不把"生命周期归 Swapchain"当成无读写冲突证明。（**帧槽 ×2 定案**：并发各用各槽 + 同槽重用隔 wait_for_slot 的 fence，两份同步证据见施工记录 §4；rebuild_depth 幂等、销毁前 wait_idle）
- [x] **3.4.4 `record_clear_and_submit` → `record_frame`**：接上传票据、必要的 acquire 屏障、清屏、直接 draw 与提交；错误返回前保持 fence/信号量/资源状态可恢复，否则优雅退出。（票据等待在 GPU 侧 @ALL_COMMANDS；D2/D6 形状不变；**acquire 屏障随 CONCURRENT 定案退役**，跨族可见性由票据信号量收口，见施工记录 §5.3）
- [x] **3.4.5 DrawList 同帧消费**：PostUpdate 传播+采集，Last 准备并提交；拓扑快照不跨 CPU 帧，GPU 执行可异步。
  - M2 可逐 draw push constants，步骤 5 迁移常驻实例表与 indirect；不宣称此时已消除 CPU 每对象装配。（逐 draw cmd_push_constants 96B + draw_indexed）
  - 原模型 `LensesMat` 为 BLEND、`HoseMat` 为双面；M2 用明确的不透明调试覆盖绘制六个 primitive，记录与生产材质的差别，不丢弃镜片后声称完整。（调试覆盖报一次即歇：blend 关、unlit 忽略、统一 Lambert；实测 6 行 alpha 均 =1，镜片照画）

## 验证

1. 两侧同源不透明调试条件下几何完整；六个 primitive、轮廓、UV 与变换可核对。（首跑截图 + 官方对照同向，[_assets](../_assets/helmet-debug-first-render.png)）
2. 重叠几何的遮挡正确；近远深度、非恒等变换、非均匀缩放的法线正确，不只检查原始头盔静态姿态。（自遮挡正确；非均匀缩放+旋转临时验证通过，截图入档，验证后已删）
3. 颜色空间无重复 gamma；base-color/unlit 测试与光照测试分开。（链路见上；分项对照归 3.5）
4. 验证层/同步验证实际开启；resize、最小化、异常返回、退出回归通过。（**定稿 + 回归全程零 VUID/WARN**；resize 900×620 重建照常画、最小化让路→还原存活、WM_CLOSE→排空→反序拆除→退出码 0）
5. main 仍只组装，编排住 host，录制住 Vulkan 模块；允许为正确性调整同步边界，不为旧结构承诺保留错误代码。（pipeline.rs 新模块、frames.rs 录制、host 只组装 DrawList）

## 材料清单

- 规则篇（2026-09-29，跨段生效）：《实现规则：渲染器跨段施工的实现要求》——本段实现要求与定案的"必须"化提炼（三方互证 / cofactor / reverse-Z 成套 / 正高度 viewport / 帧槽深度 / CONCURRENT / 探针形状覆盖消费形态 / 修结构不修验证）；**正本住用户级 ZCode rules 目录 `~/.zcode/rules/bevy-renderer-implementation-rules.md`（不入库），判例与证据在本篇施工记录**。
- 机制篇（2026-09-29）：[图形管线机制：打包清单、双集ABI与bindless下的角色](图形管线机制：打包清单、双集ABI与bindless下的角色.md)——pipeline 三问（进包什么/包外什么/多久编一次）、"每帧绑一次"与 Unity SRP Batcher 对照、UAB 在飞更新与管线无感、无长度 runtime array vs layout 容量账本、layout 公共 ABI 与 3.5 变体分岔；§0 判定线含诚实边界（M2 管线数=1）。
- 同步讲解篇（2026-09-29）：[GPU同步策略：以record_frame为例——进场屏障、提交等待与流水线阶段](GPU同步策略：以record_frame为例——进场屏障、提交等待与流水线阶段.md)——一帧七道闸门全景表（两道批间等待+三道屏障+栅栏两端+置位）、"等待是卡在阶段不是整批冻结"、**进场屏障 src 落点与提交等待作用域的耦合**（D6 教训机制化）、票据 GPU 侧等待；承接步骤 2 同步语义篇（其 §2.1 的 src=TOP_OF_PIPE 为 D6 修复前旧形状）；配图两张入 [步骤 _assets](../_assets/)。
- Barrier 家族篇（2026-09-29）：[Barrier家族：处置载荷、三种屏障与常见形状](Barrier家族：处置载荷、三种屏障与常见形状.md)——**屏障=依赖声明+随附处置指令**（布局转换是载荷、作用域是安全壳、执行四步）、UNDEFINED=作废旧内容故免单、三种屏障按绑定粒度分档、十种常见形状表（含将来必遇的离屏回采样/mipmap）、纹理上传两道屏障实拆；上传现场细节接 3.2 的 Buffer/Staging 与 Uploader 两篇；配图 [barrier-anatomy-upload](../_assets/barrier-anatomy-upload.png)。
- 施工记录（2026-09-29）：[3.4.1-3.4.5-管线与绘制施工记录：正式着色器、图形管线与DrawList同帧消费](3.4.1-3.4.5-管线与绘制施工记录：正式着色器、图形管线与DrawList同帧消费.md)——判定线收账、三方布局冻结、朝向证据链、帧槽深度定案、CONCURRENT 取舍与 **Draw-09600 三轮 bisect 排障实录**。
- 配图（2026-09-29 入 [步骤 _assets](../_assets/)）：[helmet-debug-first-render](../_assets/helmet-debug-first-render.png)（首跑完整画面）、[helmet-nonuniform-scale-rotated](../_assets/helmet-nonuniform-scale-rotated.png)（非均匀缩放+旋转）、[helmet-resize-900x620](../_assets/helmet-resize-900x620.png)（resize 重建）、[viewport-negative-inverted](../_assets/viewport-negative-inverted.png)（负高度反例）、[e5-known-clip-probe](../_assets/e5-known-clip-probe.png)（E5 光栅级实验）。

## 待决问题

- ~~深度附件按帧槽还是图像索引分配~~（2026-09-29 已决：**帧槽 ×2**，同步证据 = 槽位隔离 + wait_for_slot fence；见施工记录 §4）
- ~~正式 push constant 字段顺序与 sampler 索引布局~~（2026-09-29 已决：沿 3.3 冻结表 96B/0-64-68-80，`PushLayout` 静态断言互证；实测 6 材质 alpha 均 =1，BLEND 覆盖无可见差异）
- ~~swapchain UNORM 直出的输出编码缺位~~（2026-09-29 已决：**UNORM 底板 + SRGB view 别名**——swapchain_mutable_format 扩展 + 双格式清单 + MUTABLE_FORMAT 旗标三件套；blend 线性域、present 不经 view；回退分支（无 SRGB 支持）保留并响亮 warn。见施工记录 §3.2）
- 背面剔除与 front face 变体：正高度下绕向未被镜像，开剔除前的绕向判定与双面/BLEND 变体另立（M2 维持 cull NONE）。
- wgpu-hal 30 负高度翻转与本机实测相反的机制（wgpu 栈内补偿）：超出本项目范围，记录在案防同款直觉（施工记录 §3.3 诚实边界）。
- RenderDoc 描述符可见性验证：环境就绪后补做（承 3.3）。
