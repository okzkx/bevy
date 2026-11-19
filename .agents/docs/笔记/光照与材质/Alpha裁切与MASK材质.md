# MASK 材质：丢片元不是混合，代价随填充量走

Emerald 场景的树叶被画成方片。材质文件里标了 MASK，渲染器当时只按 `base_color.a < 1` 决定进不进透明层，factor alpha 为 1 的树叶因此整片进了不透明绘制。补上“alpha 低于阈值就丢弃片元”的裁切后，树叶正常了，阴影绘制却从约 5.7ms 涨到 24.2ms，帧率掉到 23fps。同一次改动里有两项代价：裁切让所在管线失去 early-Z（在跑片元着色器之前先做深度测试，被挡住的片元直接不跑），而且被铺到了全部四级阴影级联。

**MASK 是按阈值丢片元，不是混合；存活片元照常写深度、不需要排序，但裁切会让所在管线失去 early-Z，成本随被光栅化的片元数走，不随物体数走。**

## 三种 alpha 模式：切与混是两件事

glTF 用 `alphaMode` 区分三种材质：

| 模式 | 片元结果 | 深度 | 排序 | 典型用途 |
|---|---|---|---|---|
| `OPAQUE` | alpha 被忽略，整片画 | 写 | 不需要 | 绝大多数实体表面 |
| `MASK` | `alpha < cutoff` 的片元整个丢弃，其余按不透明画 | 存活片元写 | 不需要 | 树叶、草、花、铁丝网、镂空招牌 |
| `BLEND` | 输出 alpha 交给混合方程 | 一般不写（透明层只测不写） | 需要按物体排序 | 玻璃、烟雾、软边半透 |

表里的透明层指存半透明物体、最后混合的那一遍绘制。

`MASK` 的阈值比较对象是采样得到的 alpha：

```text
alpha = baseColorTexture.a × baseColorFactor.a
if (alpha < alphaCutoff) discard;   // 不写颜色、不写深度
```

`alphaCutoff` 来自 glTF，缺省 **0.5**（Bevy 的 glTF 加载器也是 `unwrap_or(0.5)`）。Unity 的 Cutout / Alpha Clipping、UE 的 Masked + Opacity Mask Clip Value，与 glTF 的 `MASK` 是同一件事；“MASK 裁切”“alpha test”“cutout”指的是同一种操作。

三个容易混的点：

- **MASK 不是“alpha<1 就半透”**。它只做二值判决：要么完全消失，要么完全按不透明画。哪怕 `baseColorFactor.a = 0.8`，只要 `0.8 × tex.a ≥ cutoff`，结果仍是实心。
- **MASK 与 BLEND 的分界是“是否需要逐片元混合”**。树叶用 MASK 是因为叶子边缘可以硬切；玻璃需要连续透明度，只能 BLEND。
- **`AlphaToCoverage` 在单采样下退化为 `Mask(0.5)`**。它原本靠 MSAA（多重采样抗锯齿，同一像素取多个样本）的采样覆盖率表达软边；当前管线是 1 sample，所以材质分类时直接映射成裁切。

## discard 让整条管线失去 early-Z

MASK 的裁切在着色器里是一条 `discard`（编译后的指令是 `OpKill`；SPIR-V 是 Vulkan 的着色器字节码）。片元着色器（FS，fragment shader）里一旦出现它，驱动就不能再用“先深度测试、通过才跑 FS”的顺序，因为一个片元是否写深度要等 FS 执行完才知道。整条管线退化为 late-Z：所有被光栅化的片元都要跑 FS，深度测试在之后才做。

这条约束不只影响 MASK 物体本身。如果一条管线同时画不透明和 MASK 几何，**不透明部分也陪跑 FS、也失去 early-Z**。第一版修复把裁切直接塞进唯一的 shadow 管线，代价由此被放大；实测数字见后面的版本表。

## 裁切要放在导数之后

G-buffer（延迟渲染的几何缓冲：先把 albedo、法线、材质参数写进去，光照在后一遍算）用屏幕空间导数现算切线空间基（TBN，`dpdx/dpdy`，不依赖顶点 tangent）。`discard` 会让部分调用提前退出，导数在同一组像素的辅助调用（helper invocation，为算导数而多跑的同伴调用）分歧时结果未定义。顺序必须是“先算导数，再裁切”，不能在 FS 开头就 `discard`。

## 固定 LOD 0 会让镂空边缘闪

裁切阈值对 mip（同一贴图逐级缩小的副本链）很敏感：远处一个像素覆盖多片叶子，固定 level 0 采样只能拿到其中一片的 alpha，镂空形状逐帧跳动，表现为噪点/闪烁。base color 必须走隐式 LOD（`textureSample`，由屏幕导数选 mip），让远处像素取到覆盖率平均后的 alpha。

这也是 Unity 纹理导入里“Mip Maps Preserve Coverage”要解决的问题：生成 mip 时尽量保持 alpha 覆盖率。ash_renderer 的做法是采样端用隐式 LOD；normal/MR/occlusion 仍固定 level 0，是当前已知的边界（见“边界与未做”）。

## 阴影要用同一判据，否则叶子镂空、影子方片

可见面做了裁切、shadow depth（只写深度的阴影图绘制）不裁切，就会出现“叶子是镂空的，影子却是方片”。shadow 的片元着色器要用同一个 `alpha = tex.a × factor.a` 与同一个 cutoff；这也是 MASK 材质需要一条带贴图采样的 shadow 管线的原因。

代价同样落在填充量上：级联阴影（按相机距离分层的四张阴影图）每级 2048²，地被和树冠在光空间高度重叠，`discard` 下的 overdraw（同一纹素被多个三角形重复覆盖）全部照付。裁切本身不贵，贵的是它在多大面积上被重复执行。由此得到两条做法：不透明行不进裁切管线；裁切只留在看得见细节的近级联。

## 在 ash_renderer 里，MASK 从材质记录走到两套 shadow 管线

### 材质记录与分类

64B 材质记录的尾部（第 52/56 字节）承载 alpha 语义：

```text
alpha_mode:   0 = Opaque / 1 = Mask / 2 = Blend
alpha_cutoff: Mask 的阈值，其余模式写 0
```

CPU 侧唯一口径是 `material_alpha_fields`（`ash_renderer/src/render/resident/tables/instance_table.rs`）：

- `AlphaMode::Mask(c)` → `MASK + c`；
- `AlphaMode::AlphaToCoverage` → `MASK + 0.5`（单采样退化）；
- `Blend / Premultiplied / Add / Multiply` → `BLEND`；
- `Opaque` → `OPAQUE`。

`material_is_transparent` 决定“进不进透明层”：blend 族恒进；opaque 保留 `factor alpha < 1` 的兜底（旧行为）；**MASK 恒不进**，即使 factor alpha < 1 也留在不透明绘制里做裁切。GPU 侧 `ash_renderer/shaders/common/instance.wgsl` 里的 `material_is_transparent` 与它逐条同口径。

### 五个消费方

| 位置 | 用途 |
|---|---|
| `ash_renderer/shaders/gpu_driven/build_indirect.wgsl` | 透明行在**不透明**间接命令里写 `instanceCount = 0`，避免重复画 |
| `ash_renderer/shaders/scene/transparent/transparent_collect.wgsl` | 只收集进透明层的行，MASK 不进来 |
| `ash_renderer/shaders/scene/shadow/shadow_build.wgsl` | 透明行不进 shadow；MASK 行按管线/级联分流 |
| `ash_renderer/shaders/scene/gbuffer/gbuffer_write.wgsl` | MASK 行在片元端采样 base color、低于阈值 `discard` |
| `ash_renderer/shaders/scene/shadow/shadow_write_mask.wgsl` | shadow depth 用同一裁切判据 |

任何一处漏改，都会出现“可见面与阴影/透明层的行集合不一致”的假象；`ash_renderer/src/remote` 里的 CPU 参照（cluster 的透明行计数、transparent sort check、ring census 的材质记录解码）必须同步同一口径。

### shadow 两条管线

`shadow_write` 原本只有一条 depth-only 管线（没有片元着色器，early-Z 有效）。加入裁切后拆成两条：

- **不透明管线**：无 FS，实例描述符集 + 阴影描述符集（描述符集是把 buffer、贴图绑定给 shader 的一组句柄）两套绑定，继续享受 early-Z；
- **MASK 管线**：多绑无绑定纹理表（bindless，shader 用槽号取贴图），顶点着色器（VS）把 UV 与逐图元恒定的材质索引（flat 插值）带到 FS，FS 采样 alpha 并 `discard`。

命令侧由 `ash_renderer/shaders/scene/shadow/shadow_build.wgsl` 按 `alpha_mode` 分流成两族间接命令缓冲（代码里的命令环：不透明 b6..9、MASK b10..13），绘制时每级联各发两次 indirect（间接绘制：CPU 只发一条命令，绘制参数从 GPU 写好的缓冲读）。**不透明行不付裁切成本**，这是拆分的目的。

### 级联策略

`ash_renderer/shaders/scene/shadow/shadow_build.wgsl` 里有一个策略常量：

```wgsl
const MASK_SHADOW_CASCADES: u32 = 1u;
```

- cascade 0（最近、精度最高）走 MASK 裁切，叶子影子是叶状；
- cascade 1~3 把 MASK 行**并回不透明命令缓冲按实心画**，不裁切；
- 透明行仍然完全不进 shadow。

这不是“远级联不画影子”——那会让远处的树浮空。实心回退保住接地阴影，只牺牲远处轮廓细节；而远级联 texel（阴影图上的像素）密度本来就低，镂空形状基本看不出来。

## 资产侧只看一件事：贴图 alpha 是否在抠形状

glTF 文件合法（glTF 规范允许任何材质标 `MASK`/`BLEND`）不等于语义正确。判定只看一条：**baseColor 贴图的 alpha 是否在抠形状**。

| 贴图 alpha 直方图 | 正确模式 | 说明 |
|---|---|---|
| 大量 0 与 255、中间值少（二值） | `MASK` | 典型叶片/草叶卡片 |
| 全 255（没有透明像素） | `OPAQUE` | 标 MASK 也不会触发裁切，纯浪费 |
| 连续渐变、大量中间值 | `BLEND` | 玻璃、软边半透 |
| 全 255 但 `factor alpha < 1` | `BLEND`（均匀透明） | 靠因子表达整体透明度 |

Emerald（day/dusk 各一份）按这条口径扫出来 18 处错误，两类都是典型：

- **9 个 MASK 空转**：`FlowerStem_05/06`、`Planter`、`foliage_ground_cover` 及 `_1~_4`、`Grass_Patterned`——baseColor alpha 全 255，cutoff 永远不触发。它们被送进裁切管线，视觉与 OPAQUE 相同，白付填充。
- **2 个 OPAQUE 漏裁切**：`Boston_Fern_Fiddlehead.DoubleSided`（80% 像素 alpha=0）、`Cards.DoubleSided`（78%）——贴图在抠形却被当不透明画，于是永远显示为方片。
- **7 个 BLEND 空转**：4 个 `Glass_Roof_Awning`、`Glass_Signs`、`Sheleg_Glass_Sign`、`glass`——`factor alpha = 1` 且贴图 alpha 全 255，文件里也没有 transmission/volume 扩展，没有任何可混合的内容；它们只会白占透明层。

修正工具是 `tools/fix_gltf_alpha.py`：按直方图判定（全透明像素占比阈值 5%，cutoff 固定 0.5），只改 `alphaMode`/`alphaCutoff`，支持 `--dry-run`，写盘前备份到 `.temp/gltf_alpha_backup/`，可重复执行。实跑结果：两份文件各修 18 处，再跑为 0，引擎侧 `transparent_rows_gpu` 从 18 掉到 0，零 VUID。

两个资产侧提醒：

- 这批 glTF 由 Blender 导出。脚本修的是导出产物，重导会被覆盖；根治要在 `.blend` 源里改材质，或让导出脚本按 alpha 直方图自动刷一遍。
- 引擎侧可以做兜底（MASK 但贴图无 alpha → 按 OPAQUE；BLEND 但贴图与因子都无 alpha → 按 OPAQUE），但兜底不能替代资产口径；`has_alpha` 探测（BCn 块压缩纹理烘焙里已有）可以复用。

## 这次返工：四个版本的帧时与八条教训

数字口径：第一版是用户近景机位；第二、三、四版是同一默认机位，可直接横向比。

| 版本 | shadow 实现 | gpu_shadow p50 |
|---|---|---|
| 修复前 | depth-only，无裁切（树叶方片） | 约 5.7ms（历史记录） |
| 第一版 | 唯一管线加 `discard`，4 级联全裁切 | **24.2ms**（近景）；帧率 23fps |
| 第二版 | 拆不透明/MASK 两条管线，4 级联仍全裁切 | **20.2ms** |
| 第三版 | 裁切只留 cascade 0，远级联不画 MASK | **5.0ms** |
| 第四版 | 裁切只留 cascade 0，远级联退实心 | **8.2ms**（整帧耗时 17ms，约 59fps） |

教训按“下次能直接照做”的形式写：

1. **画面问题和性能问题分开归因。** “树叶是方片”是缺裁切；“帧率掉到 23”是裁切放错位置与范围。第一版只验证了画面正确就结束，漏掉了分段帧时，代价是下一轮返工。
2. **分类跟 `alpha_mode` 走，不跟 `factor alpha` 走。** 旧代码用 `base_color.a < 1` 判透明层，同时漏掉两类：factor alpha=1 的 MASK（树叶）和 factor alpha=1 的 BLEND（Emerald 玻璃）。alpha 模式是材质声明的语义，因子只是一个乘数。
3. **加 `discard` 前先问“这条管线里还有谁”。** 只要有不透明几何共用管线，early-Z 就会被一起关掉；正确做法是把裁切隔离到独立管线，而不是接受整条管线退化。
4. **成本跟填充量走，不跟物体数走。** MASK 行只有几百条，但 4 级联的影子填充把 20ms 花掉了。判断裁切开销时看“被光栅化的片元总数”，不是看材质数量。
5. **远距离用降级，不要直接消失。** 远级联不画 MASK 能到 5.0ms，但树会浮空；补实心只多 3ms，接地阴影还在。降级代理（实心轮廓）比“没有影子”更接近正确。
6. **导数与采样顺序是正确性的一部分。** 裁切放在 TBN 导数之前会产生未定义导数；固定 LOD 0 会让远处裁切闪烁。顺序与 mip 不是优化项。
7. **资产要单独审计，合法 ≠ 正确。** 这次 18 处材质问题里，9 个 MASK 是空转、2 个该 MASK 的没标、7 个 BLEND 没 alpha。用 alpha 直方图能机械判定，脚本要幂等并留备份。
8. **GPU 分类改动要同步所有 CPU 参照。** 透明行判定同时存在于 drawlist 组装、indirect、透明收集、cluster 对照、ring census 与排序对照里；漏一处，BRP（Bevy Remote Protocol，从外部查询运行中程序的接口）对账会给出假一致或假告警。

## Unity 与 UE 的同与不同

Unity 的 URP（通用渲染管线）ShadowCaster pass 在 `_ALPHATEST_ON` 下采样 alpha 并 `AlphaDiscard`，不透明材质不采样。UE 的 Masked 材质走带透明度掩码（opacity mask）的 shadow depth 变体；VSM（虚拟阴影图）文档明确阴影页按屏幕需求分配、渲染，并跨帧缓存。两家的基础形态与拆分后的两条管线一致，差异在远距离治理手段：UE 用页面缓存摊薄，ash_renderer 目前用“远级联退实心”替代。

## 复现与验证

```bash
# 代码侧
cargo clippy -p ash_renderer --all-targets -- -D warnings
cargo test -p ash_renderer --lib

# 资产侧（先 dry-run，看清单再写盘）
python tools/fix_gltf_alpha.py --dry-run assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf
python tools/fix_gltf_alpha.py assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf

# 运行后读数（BRP 15702）
# ash_renderer/perf_stats      → gpu_shadow_ms / gpu_gbuffer_ms / frame_wall_ms
# ash_renderer/cluster_census  → transparent_rows_gpu
# ash_renderer/draw_stats      → command_count / visible_count
```

验证要两条线同时看：**读图**确认树叶/树影形状，**分段帧时**确认 shadow 没有再变成大头。只截图会漏掉 20ms 的性能回归；只读帧时会漏掉“方片”。

## 边界与未做

- **AlphaToCoverage**：需要 MSAA 才有意义；单采样下已映射为 `Mask(0.5)`。要换回 early-Z，路线是 MSAA + AlphaToCoverage，不是继续调 cutoff。
- **normal/MR/occlusion 仍固定 LOD 0**：base color 已改隐式 LOD；其余贴图的 mip 滤波尚未接，缩小视角下仍可能有细节噪点。
- **没有按物体或按材质的“不投影”开关**：Unity 的 `Cast Shadows`、UE 的逐 primitive 设置在资产层控制阴影成本；当前只有级联策略。
- **没有阴影页缓存/距离场**：UE5 的 VSM 用“只渲染可见页 + 跨帧缓存”摊薄 Masked 阴影成本；远级联实心是当前架构下的务实替代。
- **透明层不参与 shadow**：blend 族行完全不进 shadow depth，这一点没有随裁切改动。

## 来源

- 材质分类与记录布局：`ash_renderer/src/render/resident/tables/instance_table.rs`
- G-buffer 裁切：`ash_renderer/shaders/scene/gbuffer/gbuffer_write.wgsl`
- 共享 alpha 判定：`ash_renderer/shaders/common/instance.wgsl`
- shadow 两管线与命令缓冲：`ash_renderer/src/render/passes/shadow/shadow_map.rs`、`ash_renderer/shaders/scene/shadow/shadow_build.wgsl`、`ash_renderer/shaders/scene/shadow/shadow_write_mask.wgsl`
- 资产修正工具：`tools/fix_gltf_alpha.py`
- 相关提交：`cc25cb005`（补齐 MASK 裁切）、`0094b1c0f`（拆分 shadow 管线）、`54c65b34d`（base color 隐式 LOD）
- [Unity URP ShadowCasterPass.hlsl](https://github.com/Unity-Technologies/Graphics/blob/master/Packages/com.unity.render-pipelines.universal/Shaders/ShadowCasterPass.hlsl)：shadow caster 在 `_ALPHATEST_ON` 下调用 `AlphaDiscard` 做裁切。
- [Unreal Engine Virtual Shadow Maps](https://dev.epicgames.com/documentation/en-us/unreal-engine/virtual-shadow-maps-in-unreal-engine)：阴影页按屏幕需求渲染，并在帧间缓存。
