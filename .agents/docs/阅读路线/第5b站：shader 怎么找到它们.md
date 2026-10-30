# 第5b站：shader 怎么找到它们

本篇讲第 5 站下半：数据进了显存之后，shader 怎样按下标一路找到网格引脚、模型矩阵、材质参数、贴图和光源。数据怎么进显存见《[第5a站：bindless 数据怎么进显存](第5a站：bindless%20数据怎么进显存.md)》。

> 进度：讲解正本已写好（2026-10-30），待用户逐段阅读校验。依据同第5a站（main=51884896a 加工作区未提交的 `bindless_texture_table.rs` 改名、`slot_allocator.rs` 与 `frame_uniform_table.rs` 拆出）。代码引用写符号名不写行号。

## 总图：一次不透明绘制的取数链

```text
indirect 命令（build_indirect.wgsl 写）
   firstInstance = 实例槽号 slot
        │  instanceCount = 1，所以 VS 的 @builtin(instance_index) == slot
        ▼
set2 实例表   objects[slot] ──► 引脚已在命令里；material_index
              models[slot]  ──► 世界变换
              materials[material_index]
                 ├─ base_color / metallic / roughness / reflectance
                 └─ tex_base_color · tex_normal · tex_mr · tex_occlusion · sampler_index
                        │
                        ▼
set0 bindless 表   textures[tex_*]，samplers[sampler_index]
set1 帧 UBO        frame.view_proj 等全局量

合成 pass：clusters[格子].indices[k] ──► lights[槽号]（光源分簇集 b1 = 光源表）
```

**整条链没有一个逐对象的 push 常量**：CPU 不给每个 draw 推参数，所有“这个物体用什么”都编码成表里的下标，shader 按下标自己去查。这就是 bindless 在本项目里的实际含义——不止贴图，网格引脚、矩阵、材质全是按槽号直取。

## 一、set 声明：一份静态表管到底

`core/binding/set_layout.rs` 的 `SetLayoutDesc` 是一个描述符集布局的**唯一一份**声明：名字、每个 binding 的编号/种类/数量/可见阶段/binding 旗标、布局旗标。从它推导出四样东西：

| 推导物 | 入口 | 作用 |
|---|---|---|
| Vulkan 布局 | `create` | 先跑 `problems` 自检，再建 `VkDescriptorSetLayout`，binding 旗标经 `BindingFlagsCreateInfo` 挂入 |
| descriptor pool 容量 | `pool_sizes_vec` / `merged_pool_sizes` | 按种类累加 `count × 集数`，不再手写 pool 尺寸 |
| UAB pool 旗标 | `needs_uab_pool` | 布局带 `UPDATE_AFTER_BIND_POOL` 时 pool 也必须带 `UPDATE_AFTER_BIND` |
| 写入与对账依据 | `DescriptorWriter`、`PipelineDesc::problems` | 写描述符和建管线时按它校验 |

`problems` 自检的条目：binding 编号不重复、数量非 0、阶段非空；带 `UPDATE_AFTER_BIND` 的 binding 要求布局有 `UPDATE_AFTER_BIND_POOL`（VUID-03000 三层配套中的布局层）；**UAB 只允许用在 sampled image 和 sampler 上**，因为 `Context` 只启用了 `descriptorBindingSampledImageUpdateAfterBind`。

**带身份的句柄**：

- `SetLayout`：布局句柄 + 它的声明。资源族建好后借给管线组装，`SetLayouts` 按声明的**指针身份**取句柄（同名不同份也会被拒）。
- `DescSet`：描述符集句柄 + 它的声明。录制绑定时 `assert_set_sequence` 拿管线配方核对“第 i 个集是不是配方要的那份声明”，debug 构建绑错当场 panic，release 零成本。调用点在 `core/record/graphics.rs` 和 `core/record/compute.rs`。

**set 编号不是声明自带的属性**，而是它在管线配方 `PipelineDesc::sets` 里的位置。同一份声明在不同管线里编号不同：

| 管线配方 | set0 | set1 | set2 | set3 |
|---|---|---|---|---|
| G-buffer 写入 | bindless | frame | instance | — |
| 透明前向 | bindless | frame | instance | light_clusters |
| 合成（全屏光照） | frame | gbuffer_sampled | light_clusters | shadow |
| cull / LOD / build_indirect | instance | （cull 另有 shadow） | — | — |
| overlay（egui） | bindless | — | — | — |

所以“set0 = bindless 表、set1 = 帧 UBO、set2 = 实例表”是**场景几何管线**的约定，WGSL 里的 `@group(n)` 必须与该管线配方的位置对上，这一对账在第 6 站的 `PipelineDesc::problems` 里做。

## 二、按声明校验的写入

`core/binding/descriptor_writer.rs` 的 `DescriptorWriter` 是全库写描述符的统一出口：先收集，`submit` 时一次校验、一次 `vkUpdateDescriptorSets`。

- **逐条校验**：binding 存在；写入形状和声明种类匹配（buffer 只能写进 uniform/storage buffer，image 只能写进 sampled/storage image，sampler 只能写进 sampler）；数组元素在声明数量以内。
- **完备性校验**：非 `PARTIALLY_BOUND` 的 binding 必须写满全部元素——布局是共享超集，集是整份绑定的，漏一个就是绑定时读未定义描述符。
- 描述符类型以声明为准，不从写入形状猜。
- 任何一条不过，整批拒写，报错里带声明名和 binding 号，而不是等验证层报 VUID。

**bindless 表怎样同时满足完备性和“从不写满”**：它的两个 binding 带 `PARTIALLY_BOUND`，完备性检查直接跳过；但每次 `publish` 写单个槽时，种类和元素上界照查。`storage_sets::create_sets`（光源分簇、透明层共用）、`FrameUniformTable::new`、`InstanceTables::new` 都走同一个写入器，且都要求写满。

## 三、四张表各管什么

| 表 | 声明 | 描述符写几次 | 内容怎么更新 | shader 侧名字 |
|---|---|---|---|---|
| bindless 表 | `BINDLESS_TEXTURE_SET_DESC`：b0 贴图 ×1024、b1 采样器 ×1024，UAB + PARTIALLY_BOUND | **运行中按槽写**：每次 `publish` 写一个新槽 | 不改已发布槽，换内容 = 发布新槽 + 旧槽延迟回收 | `textures`、`samplers`（`binding_array`） |
| 帧 UBO | `FRAME_UNIFORM_SET_DESC`：b0 UBO 208 B | 每帧槽 init 写一次 | `draw_frame` 每帧整块重写当前帧槽的 UBO | `frame: FrameUniforms`（WGSL 数据结构） |
| 实例表 | `INSTANCE_SET_DESC`：b0..b9 storage | 每帧槽 init 写一次，**永不更新** | 对象表走上传批次；各环按帧槽写或由 compute 写 | `objects`、`models`、`materials` 等 |
| 光源表 | 不自带集；挂在 `LIGHT_CLUSTER_SET_DESC` 的 b1 | 随光源分簇集建立，resize 时重建 | 脏行写到当前帧槽的环 | `lights: array<LightRecord>` |

### bindless 表（`resident/bindless_texture_table.rs`）

- **槽 0 是 fallback**：建表时上传 1×1 白图并配线性采样器，占住贴图槽 0 与采样器槽 0，永不回收。`PARTIALLY_BOUND` 下没写过的槽不能被访问，缺贴图的材质就指向槽 0。
- **`publish`**：贴图槽 `alloc` 一个新号；采样器按 `SamplerKey`（完整采样参数）去重，已有同参数采样器就引用数加一，否则分槽并创建；最后用写入器写两条描述符，返回 `SlotBinding { texture, sampler }`。
- **`retire` / `drain_retired_slots`**：贴图槽进待回收账；采样器引用数减一，归零时槽和句柄一起进待回收账。`flush_uploads` 开头等全部帧槽 fence 后，槽才回到空闲栈、采样器才销毁。
- **只往新槽写**：UPDATE_AFTER_BIND 允许集在绑定之后更新描述符，但本项目不覆盖在飞 draw 可能还在读的槽。
- **建表先核对限额**：UAB 限额是单独一套（`PhysicalDeviceDescriptorIndexingProperties`），不看普通的 `maxPerStageDescriptorSampledImages`；不够 1024 就报错，不缩容量。
- 坑：`ImageSampler::Default` 解析成线性采样器，与槽 0 同参数，所以普通贴图、fallback、egui 图集会共用采样器槽 0；采样器能否回收只能看本表的引用数。

### 槽位分配器（`resident/slot_allocator.rs`）

纯 CPU 账：分配先取空闲栈，栈空才推进游标 `high_water`，满了返回 `None`。归还分两种，按“GPU 还会不会读旧内容”选：

- `release`：立即可复用。用于从未发布给 GPU 的槽，或每个帧槽各有一份数据的表（光源表）。
- `retire` + `drain_retired`：先进待回收账，调用方等 GPU 用完再归还。bindless 表和对象表用这个。

### 帧 UBO 表（`resident/frame_uniform_table.rs`）

每帧槽一个 208 B 的 UBO 和一个集，init 时把 UBO 写进集后描述符不再动。`draw_frame` 在 `wait_for_slot` 之后调 `write_frame_ubo` 整块重写当前帧槽：`view_proj`、`dir_to_light`、环境光、方向光颜色、`mode`、`inv_view_proj`、`camera_pos`。**方向光走这里，不进光源表**。字节偏移由 `FrameUniformTableLayout` 的 `offset_of!` 静态断言钉住。

### 实例表（`resident/instance_table.rs`）

一块对象表 + 九种帧槽环，每帧槽一个 set2，十个 binding 在 init 时一次写满。b0 两个帧槽都指向同一块对象表；b1..b9 指向各自帧槽的环。

| binding | 内容 | 记录 | 写者与写法 | 读者 |
|---|---|---|---|---|
| b0 | 对象表（DEVICE_LOCAL，131072 槽） | 48 B：`first_index`、`index_count`、`vertex_base`、`material_index`、局部包围盒 | 上传批次；换内容走新槽 + 旧槽停尸房 | VS、compute |
| b1 | 模型环 | 64 B 矩阵/槽 | `draw_frame` 增量写脏行，新槽两帧各写一次 | VS |
| b2 | 材质环（65536 槽） | 64 B：PBR 标量 + 四个贴图槽 + `sampler_index` | `draw_frame` 每帧全量重写可绘材质 | FS、build_indirect |
| b3 | 剔除可见标记 | u32/槽 | cull compute | build_indirect |
| b4 | 在场环 | 可绘行的实例槽号列表 | `draw_frame` 全量重写 | build_indirect |
| b5 | indirect 命令环（DEVICE_LOCAL） | 20 B 命令 | build_indirect | `cmd_draw_indexed_indirect` |
| b6 | 遮挡标记 | u32/槽 | 遮挡测试 compute | 诊断读数 |
| b7 | LOD 变体表 | 每槽 4 档 × 12 B 引脚 | `draw_frame` 增量写脏行 | build_indirect |
| b8 | LOD 档位 | u32/槽 | 选档 compute | build_indirect |
| b9 | 缺口标记 | u32/槽 | cull 清零、build_indirect 置位 | 驻留管理器 |

两个关键点：

- **实例槽号是全表共用的索引空间**：对象表、模型环、各种标记环都按同一个实例槽直址；材质环用另一套材质槽号（bump-only，不回收，量级 = 唯一材质数）。
- **材质记录里的贴图槽在每帧写环时现查**：`write_material_ring` 从 `ImageCache::slots` 取当前槽位。base color 缺贴图或未驻留时用 fallback 槽 0；其余三张缺席写 `NO_TEXTURE_INDEX`（`u32::MAX`），shader 分支跳过采样。所以贴图换槽后，材质环下一次写入就指向新槽，不需要单独的失效通道。

### 光源表（`resident/light_table.rs`）

256 槽 × 64 B × 2 帧槽，`HOST_VISIBLE` 的 `Storage` 角色。它**没有自己的描述符集**：`LightClusters` 建集时把 `LightTable::ring_buffer(slot)` 写进 `LIGHT_CLUSTER_SET_DESC` 的 b1，b0 是 cluster 参数、b2 每格列表、b3 每格计数。

- 记录 64 B：`pos_radius`（w = 半径，0 = 死灯）、`color_intensity`、`flags`（bit0 阴影、bits 8..15 类型）。
- `spawn`/`update`/`remove` 改 CPU 镜像并记脏，`draw_frame` 在 `wait_for_slot` 之后调 `write_dirty(slot)` 写当前帧槽的环。
- 删除时槽位**立即**归还并写死灯记录，不进停尸房：cluster 列表存的是槽号，一帧内旧列表引用到已换内容的槽，最坏是一次视觉闪变，不会读到悬空数据。
- 只装点光（spot 预留记录形状），方向光走帧 UBO。
- 现状约束：脏账记“剩余写次数”而非“已写哪些槽”，同一槽号连续两次 `write_dirty` 会漏掉另一帧槽，详见《[CPU 与 GPU 异步访问：三种存储与更新策略](CPU与GPU异步访问：三种存储与更新策略.md)》方式二。

## 四、shader 侧怎样对上

以 `gbuffer_write.wgsl` 和 `common/mesh.wgsl` 为例：

```wgsl
@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;
```

1. `build_indirect.wgsl` 遍历在场环，对每个槽写一条命令：引脚取自 LOD 选档结果（缺档回退到对象记录的基准引脚），`firstInstance = slot`，不可见或透明的 `instanceCount = 0`。
2. `mesh_vertex` 用 `@builtin(instance_index)`（等于 `firstInstance`）读 `objects[slot]` 和 `models[slot]`，把 `material_index` 以 flat 插值传给片元。
3. `sample_surface` 读 `materials[material_index]`，用 `textures[mat.tex_*]` 和 `samplers[mat.sampler_index]` 采样。
4. 合成 pass 按像素算出 cluster 格子，读 `clusters[格].indices[k]` 得到光源槽号，再读 `lights[槽号]`；`pos_radius.w <= 0` 的死灯跳过。

**跨端布局三道防线**：

- Rust 侧 `*Layout` 镜像结构 + `offset_of!` 静态断言（`FrameUniformTableLayout`、`ObjectRecordLayout`、`MaterialRecordLayout`），打包函数按断言表手工排字节。
- `build.rs` 的 `HOST_LAYOUT_GATE` 在编译 shader 时检查 WGSL 结构体的 naga span：`MaterialRecord` 64、`ObjectRecord` 48、`LightRecord` 64、`LodEntry` 12、`IndirectCommand` 20，不符直接构建失败。坑：WGSL 垫尾要用标量 `u32`，用 `vec3<u32>` 会因 16 字节对齐把步长撑大，整张表错位。`FrameUniformTable`（208 B）不在这张门里，只靠 Rust 侧静态断言和描述符 range。
- 管线建立前 `PipelineDesc::problems` 拿 shader 实际用到的 `(set, binding, 种类, 数组形状)` 对账：runtime 长度数组（bindless 表）必须带 `PARTIALLY_BOUND`（第 6 站细读）。

**设备特性**：`Context` 显式启用 `runtime_descriptor_array`、`shader_sampled_image_array_non_uniform_indexing`、`descriptor_binding_sampled_image_update_after_bind`、`descriptor_binding_partially_bound`。naga 输出 runtime 数组时不发射 `RuntimeDescriptorArray` capability，由 `build.rs` 在 spirv-val 拒绝后补丁注入。

## 五、现状约束

- **一个材质只有一个采样器下标**：`sampler_index` 取自 base color 贴图的发布结果，normal、metallic-roughness、occlusion 三张图采样时也用它。三张图的采样参数和 base color 不同时，会按 base color 的参数采样。这是现役形状，不是已修复的保证。
- **贴图 mip 钉 level 0**：`sample_surface` 用 `textureSampleLevel(..., 0.0)`，GPU 生成的 mip 链目前不参与采样。
- **容量都是硬上限**：bindless 1024、对象表 131072、材质 65536、光源 256，用尽报 `Capacity` 错误，不截断。

## 校验点

- bindless 槽位“按槽发布、从不写满”怎样同时通过写入器检查：`PARTIALLY_BOUND` 免完备性检查，单槽写入仍查种类与上界。
- UAB 为什么只开在 sampled image / sampler 上：对照第 2 站 `Context` 实际启用的特性；`SetLayoutDesc::problems` 把这条写成了自检。
- set1 帧 UBO、模型环、材质环、光源环的写入是否都在 `wait_for_slot` 之后（第 3 站帧槽模型）。
- 实例表描述符为什么可以 init 一次写死：buffer 容量固定、不重建，槽位用尽报错；Hi-Z 采样视图随 swapchain 换图，所以不进 set2，另建小布局。
- 同一份声明在不同管线里的 set 编号不同，WGSL 的 `@group(n)` 对的是配方位置。可用 BRP 只读方法 `ash_renderer/layout_contract` 现场查看声明表。

## 相关源码

- `ash_renderer/src/render/core/binding/`：`set_layout.rs`、`descriptor_writer.rs`、`storage_sets.rs`；`pipeline_layout.rs` 的对账口径归第 6 站。
- `ash_renderer/src/render/resident/`：`bindless_texture_table.rs`、`slot_allocator.rs`、`frame_uniform_table.rs`、`instance_table.rs`、`light_table.rs`。
- `ash_renderer/src/render/passes/lighting/cluster.rs`：`LIGHT_CLUSTER_SET_DESC` 与光源表 b1 的接线。
- `ash_renderer/src/driver/host/drawlist.rs`：`write_material_ring`（第 9 站细读）。
- `ash_renderer/shaders/`：`common/instance.wgsl`、`common/mesh.wgsl`、`common/frame.wgsl`、`common/cluster_lighting.wgsl`、`gbuffer_write.wgsl`、`build_indirect.wgsl`；`ash_renderer/build.rs` 的 `HOST_LAYOUT_GATE`。
