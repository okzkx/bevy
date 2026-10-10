# 施工 3.5：光照与同屏对照

对应[施工计划 §0](../施工计划：Bindless起步五段拆解.md)，本段完成步骤 3 收官。

**目的**：用受控而非混杂的画面对照证明几何、纹理与光照方向正确，整理可复跑证据。

**状态：✅ 四任务收官（2026-09-29），证据见[施工记录](3.5.1-3.5.4-光照与同屏对照施工记录：相机灯光UBO、材质三模式与官方对照收官.md)。**完整透明与 PBR 不属于 M2；不能据此降低几何检查要求，也不能把简化结果宣称为原始材质等价。**步骤 3（M2）全段收官。**

## 定案（2026-09-29 施工前钉板）

- **set1 UBO 扩到 128B**：`FrameUniforms = view_proj(mat4) + dir_to_light(vec4) + ambient_color(vec4) + light_color(vec4) + mode(u32)`，偏移 0/64/80/96/112、总长 128（Rust 镜像 `offset_of!` 断言与 WGSL 三方互证，沿 96B push 的同款纪律）。binding 形状（set1 b0 UNIFORM_BUFFER）与 pipeline layout 不变——range 在写入时的 DescriptorBufferInfo，扩容不动 ABI。
- **光照数值约定与 bevy GPU 侧同款**（`bevy_pbr/src/render/light.rs` prepare_lights 逐字段对齐）：`dir_to_light = GlobalTransform::back()`（实体 forward 取负，”N·L 就绪”方向）；`light_color = LinearRgba(color) × illuminance`；`ambient_color = LinearRgba(color) × brightness`。末段亮度收敛在 shader 常量：Lambert 除 π、曝光 `exp2(-9.7)/1.2`（bevy `Exposure::BLENDER` 默认 EV100=9.7），最终 `albedo × (direct + ambient) × exposure`——与官方 `view.exposure × (direct + indirect)` 的链路同构，Burley 漫反射/镜面/IBL 未对齐项在收官记录列明。
- **取数优先级**：环境光 = 相机组件 `AmbientLight`（若挂）> `GlobalAmbientLight` 资源 > 缺席 warn+零；方向光取第一盏（多灯 warn 一次），无灯 warn+直射项置零。照相机 view_proj 同款在 Last 直读（灯与相机同为”取景/光照数据”，不进 CollectedScene 渲染拓扑快照）。
- **材质三模式**（UBO `mode`，env `ASH_RENDER_MODE` 选）：0=Lambert（默认，接 UBO 灯光）、1=Unlit（albedo 直出，base color/UV 对照）、2=Normal（世界法线可视化，法线方向验收仪器）。不透明调试覆盖（blend 关、BLEND 镜片照画）不变。
- **对照载体（待决问题定案）**：`ash_renderer/examples/official_reference.rs`——独立进程跑完整官方 bevy_render/wgpu（本 crate 的 bevy 依赖默认 feature 本就全开，main 只是运行时禁插件），不为”同屏”重构宿主。条件固定：同资产、同六 primitive、同相机参数、同方向光（20000 lx、同旋转、shadow 默认关）、1280×720、Tonemapping::None + DebandDither::Disabled + Msaa::Off + 无 Hdr/环境贴图/雾；`OFFICIAL_REF_MODE=unlit` 时全部 StandardMaterial 置 unlit。6s 自动退出便于脚本化截图。
- **截图方法**：PowerShell PrintWindow（PW_RENDERFULLCONTENT）按窗口标题抓图（全屏截图会被前台应用遮挡的方法论钉子承 3.2），Python PIL 并排合成。

## 任务清单

- [x] **3.5.1 相机与光照 UBO**：读取 view/projection、方向光、全局 `GlobalAmbientLight`，按需要处理相机组件 `AmbientLight` 覆盖；每帧在飞一份，等旧使用完成后写映射内存。（UBO 64B→128B 五偏移三方互证；数值链与 bevy prepare_lights 同源；在飞安全 = wait_for_slot fence）
  - 方向光方向来自实体 forward（UBO 存 `back()` = 表面到光，bevy GPU 同款）；明确 Lambert 中采用”光传播方向”还是”表面到光”的向量，避免符号混淆。（**已钉：存表面到光**）
  - 非 coherent 映射内存按需 flush；帧槽复用由完成条件保护，不只靠轮转计数。（GpuBuffer::write 内建 atom flush 分流；本机 coherent 免 flush，分支证据在日志）
- [x] **3.5.2 材质策略**：保留统一的不透明调试覆盖、base-color/unlit 测试与简单 Lambert 模式。（三模式经 UBO mode：lambert/unlit/normal）
  - FlightHelmet 镜片 BLEND 不静默丢弃，几何对照中两侧统一覆盖为不透明。（6 行 alpha 均 =1 照画）
  - 记录双面、透明、metallic/roughness、normal map、曝光/后处理的简化或未实现项；完整材质不强塞步骤 4。（施工记录 §6 诚实边界）
- [x] **3.5.3 对照载体与条件**：优先独立官方 Bevy 进程配合并排截图，避免让自研进程初始化 wgpu。（`examples/official_reference.rs` 独立进程，6s 自动退出）
  - 固定资产版本、六个 primitive、相机、分辨率、viewport、材质覆盖与灯光方向。（条件表见施工记录 §3）
  - 几何、base color/UV、法线/方向光分别验收；不直接拿官方完整 PBR 默认画面要求像素相同。（[unlit 并排](../../_assets/helmet-compare-unlit.png) / [lambert 并排](../../_assets/helmet-compare-lambert.png)：轮廓/色相/亮侧方向一致）
- [x] **3.5.4 收官文档**：记录终态、同步/生命周期契约、坑和证据；更新路线图、入口与五段 README；未关闭缺陷不得隐藏在”全部通过”之下。（回归轮实抓 3.4 遗留 D7=VkShaderModule 泄漏，已修，退出路径零 VUID）

## 验证

1. 六个 primitive 在统一不透明调试模式下完整，无错误深度遮挡或变换。
2. base color/UV/采样和颜色空间通过独立受控对照。
3. 法线和光照方向正确，非均匀缩放案例成立；未对齐的 PBR/曝光差异明确列出。
4. 静态 mesh/贴图不每帧重复上传；合批、descriptor indexing 与同帧提交落点正确。
5. 验证层及同步验证确实开启，目标路径无告警；resize、最小化、失败分支、退出均有复测证据。
6. 两 Tier 纪律成立，可恢复错误才跳帧；源码修复状态与[缺陷文档](../材料/已实现缺陷与修复验收.md)一致。

## 材料清单

- 施工记录（2026-09-29）：[3.5.1-3.5.4-光照与同屏对照施工记录：相机灯光UBO、材质三模式与官方对照收官](3.5.1-3.5.4-光照与同屏对照施工记录：相机灯光UBO、材质三模式与官方对照收官.md)——§0 判定线逐条对账、UBO 128B 数值链（bevy prepare_lights 同源逐字段）、材质三模式、官方对照条件表与分项结论、D7 发现与修复、回归矩阵、诚实边界与给后续段的接口。
- 配图（2026-09-29 入[步骤 _assets](../../_assets/)）：[helmet-compare-unlit](_assets/../_assets/helmet-compare-unlit.png)（unlit 并排：base color/UV/几何）、[helmet-compare-lambert](../../_assets/helmet-compare-lambert.png)（lambert vs 官方 PBR：光照方向）、[helmet-normal-debug](../../_assets/helmet-normal-debug.png)（法线可视化）、[helmet-nonuniform-normal](../../_assets/helmet-nonuniform-normal.png) / [helmet-nonuniform-lambert](../../_assets/helmet-nonuniform-lambert.png)（非均匀缩放探针，验证后已删）、[helmet-regression-resize-900x620](../../_assets/helmet-regression-resize-900x620.png) / [helmet-regression-restore](../../_assets/helmet-regression-restore.png)（窗口回归）。

## 待决问题

- ~~官方对照采用现有示例的受控改版还是专用最小例子~~（2026-09-29 已决：专用最小 example `examples/official_reference.rs`，独立进程，条件表见上方定案；不为”同屏”重构宿主）
- 曝光/tonemap 的亮度绝对值不对齐官方（官方默认 TonyMcMapface，本段两侧 None+线性削顶）：方向/明暗界线可判读即达标，绝对亮度对齐归后处理专题。
- ~~RenderDoc 描述符可见性验证：承 3.3/3.4，环境就绪后补做（非本段判定项）。~~（2026-10-10 用户拍板：**不做**，登记为[拓展想法](../README.md)）
