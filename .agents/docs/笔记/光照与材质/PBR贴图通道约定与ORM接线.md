# PBR 贴图通道约定与 ORM 接线

Emerald 的公交玻璃和大楼窗户看起来"不透明、没反射"。查下去发现不是一个问题，而是两件事：**玻璃不透明是源资产的设计**；真正坏掉的是 **metallicRoughnessTexture 从来没接上**——220 个材质的粗糙度全是同一个常数，金属度要么被清零、要么按名字写成 1.0，那张本该提供 G/B 的 Specular 图被错挂到了 `KHR_materials_specular`。

**同一批 RGB 字节，在 ORCA 源资产里是 ORM（R=AO、G=Roughness、B=Metallic）。转成 glTF 时必须接 `metallicRoughnessTexture` 并把 `metallicFactor`/`roughnessFactor` 置 1；接到别处等于没接。**

## 一张图，两种命名

ORCA 的 Emerald Square / Bistro 源资产是 FBX + DDS + Falcor `.pyscene`。`assets/external/orca/*/README.txt` 写明了贴图通道约定：

| 通道 | ORCA 源资产 | glTF 对应 |
|---|---|---|
| R | Occlusion | `occlusionTexture`（本批不接，见下） |
| G | Roughness | `metallicRoughnessTexture.g` |
| B | Metallic | `metallicRoughnessTexture.b` |
| A | —— | —— |

BaseColor 是 RGB + Alpha=Opacity；这批资产的 Alpha 绝大多数是 255（见"玻璃为什么仍然不透明"）。

源文件把这张图叫 **Specular**，容易误导：它存的不是 F0 颜色，而是三个数据通道。glTF 没有"三合一打包图"的专用槽，只能拆成两个引用：`metallicRoughnessTexture` 用 G/B，`occlusionTexture` 用 R。同一张 PNG 接两个槽是合法且常见的做法（`assets/models/FlightHelmet/FlightHelmet.gltf` 里 `_OcclusionRoughMetal.png` 就同时挂了两处）。

## 转换链断在哪

这批 glTF 不是下载的，是我们自己的 Blender CLI 转换产物（`Khronos glTF Blender I/O v5.2.41`）。断点在材质翻译：

- **粗糙度变成常数**。Blender 的 FBX 导入器按 `roughness = 1 - sqrt(Shininess)/10` 换算，FBX 里 `Shininess = 20` 得到 `0.5527864045`——和 glTF 里 220 个材质共用的 `roughnessFactor: 0.5527864098548889` 逐位对得上。这是"贴图没进管线、只剩一个全局常数"的指纹。
- **金属度被清零**。FBX 的 metallic 普遍不可信，转换脚本按名字是否含 `metal` 粗暴置 0/1，写进 `metallicFactor`。贴图不接时，这个 0 就是最终值。
- **ORM 图挂错槽**。FBX 导入器把 Specular 贴图接到 Principled 的 Specular 输入，导出器把它写成 `KHR_materials_specular.specularTexture`。转换脚本从头到尾没读过 Specular DDS，`metallicRoughnessTexture` 一直是空的。

修复前的账面（四份资产完全一致的结构病）：

| 文件 | 材质 | 有 MR 贴图 | 有 specularTexture | extensionsUsed |
|---|---|---|---|---|
| EmeraldSquare_Day | 220 | 0 | 220 | `KHR_materials_specular` |
| EmeraldSquare_Dusk | 222 | 0 | 216 | `KHR_materials_specular` |
| BistroExterior | 132 | 0 | 131 | `KHR_materials_specular` |
| BistroInterior_Wine | 74 | 0 | 67 | `KHR_materials_specular` |

少数材质只有 `specularColorFactor: [2,2,2]`、没有贴图（水、冰、酒、自发光窗等）；它们没有 ORM 图可接，保持原样。

## 误挂的代价

- **对 ash_renderer**：`KHR_materials_specular` 不解析，材质记录里的 `tex_metallic_roughness` 是 `NO_TEXTURE`，粗糙度恒为 `0.5528`（再被 `MIN_PERCEPTUAL_ROUGHNESS` 夹住），金属度恒 0。玻璃的 `G = 0`（镜面级光滑）完全丢失。
- **对合规查看器**：`KHR_materials_specular` 的 RGB 被当成 F0 颜色。这批图 R 通道全是 0，高光颜色会偏掉；A 通道全是 255，`specularFactor = 1` 倒是中性值，删掉无损。
- 不是所有材质都"受害明显"：`Bus_Body` 的 G=150（0.588）和常数 0.5528 很接近，车漆几乎没变；变化最大的是 G≈0 的玻璃幕墙和 G≈0.96 的遮阳篷。

## 玻璃为什么仍然不透明

这条要单独钉死，因为"顺手加个 BLEND"是最容易做错的一步。证据链：

1. **贴图 alpha**。按 BaseColor PNG 的 alpha 直方图扫：Emerald Day 131 张里 34 张带非全 255 alpha，全部是叶片/花卡片；Bistro Exterior 12 张（6 张 Foliage + 招牌字、窗帘、脏玻璃遮罩、瓶贴）；Bistro Interior 8 张（灯罩、桌布、植物、盘子细节）。公交和大楼的玻璃贴图 alpha 全是 255。
2. **FBX 材质**。源 `Material` 的 `Opacity = 1.0`。
3. **Falcor 语义**。Falcor 4.0 的材质在 base color 贴图带 alpha 时走 `AlphaModeMask`，否则 `AlphaModeOpaque`；它没有 glTF 的 BLEND 通道。
4. **官方参考图**。NVIDIA 的 Emerald Square 参考渲染里，车窗和楼窗就是暗的、不透明的。

所以那轮 alpha 修正把玻璃的 BLEND 标回 OPAQUE 是对的，不存在"丢失的透明度"。玻璃真正丢的是 **roughness=0 的镜面感**——这恰好由 ORM 接线补回来，而不是靠 alpha。

## 接线规则

工具 `fix_gltf_orm.py` 对每个材质做三件事：

1. 把 `KHR_materials_specular.specularTexture` 原样（含 `texCoord` 与贴图变换）复制到 `pbrMetallicRoughness.metallicRoughnessTexture`；
2. `metallicFactor` 与 `roughnessFactor` 置 `1.0`，数值全部交给贴图；
3. 删掉该材质的 `KHR_materials_specular`。全部材质都不再使用时，`extensionsUsed` 里的条目一并摘掉（Emerald Day 就属于这种）。

两条禁区：

- **不接 `occlusionTexture`**。这批图 R 通道全 0（三份资产、上百张图，无一例外），接上就是 AO=0；而 `ash_renderer/shaders/common/brdf.wgsl` 把 occlusion 乘在**直接光**上，画面会整体变黑。要接真 AO，先得讨论"AO 只乘环境光"这一层语义。
- **只动带 `specularTexture` 的材质**。只有 `specularColorFactor` 的水/冰/酒/发光窗不碰，它们本来就不依赖 ORM 图。

实跑结果：四份文件合计接 634 处（220 + 216 + 131 + 67）。幂等——再跑一次是 0 处。

## 工具与流程

目录 `ash_renderer/tools/fbx_to_gltf/`：

| 文件 | 职责 |
|---|---|
| `fbx2gltf.py` | 宿主侧 CLI：`convert`（调 Blender + 串后处理）、`fix-orm`（单独补接线） |
| `blender_convert.py` | 在 Blender 内执行：FBX 导入、metallic/alpha 接线修正、导出 GLTF_SEPARATE |
| `fix_gltf_orm.py` | ORM 接线修复本体，可独立运行 |

```bash
# 只修已有资产（幂等，写盘前备份到 .temp/gltf_orm_backup/）
python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py fix-orm \
    assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf

# 从 FBX 重新导出（Blender 在 PATH 或 --blender 指定；自动串 fix-orm 与 alpha 修正）
python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py convert \
    assets/external/orca/EmeraldSquare_v4_1/EmeraldSquare_Day.fbx \
    assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf
```

本次没有重跑 Blender——只对已导出的 glTF 做后处理，所以结论不依赖 Blender 版本。重导以后用 `convert`，三步（Blender 导出 → ORM 接线 → alpha 语义）一次跑完。

## 验证

三层各看一条：

**JSON 账面**：四份文件 MR 贴图数 = 接线数，`metallicFactor`/`roughnessFactor` 全为 1.0，`occlusionTexture` 数为 0；接线的贴图 URI 全部指向 `*_Specular*.png`；Day 的 `extensionsUsed` 已清空删除，其余三份保留给只有 factor 的材质。

**运行期材质记录**（`ash_renderer/ring_census`）：Emerald Day 重启后 2479/2479 个实例行的材质记录都带 MR 贴图槽，`material_all_match` 全真、零不一致，日志零 VUID。ORM 图按 **线性** 驻留：glTF loader 按用途把它标成 non-sRGB，`image_spec.rs` 映射成 `R8G8B8A8_UNORM`；BCn 烘焙里 `TextureRole::MetallicRoughness => None`，当前不压缩。

**画面**：切 `roughness` 调试视图，路面、斑马线、车体、窗面呈现不同的灰度图案（贴图在驱动数值）。修复前这里应是一整片均匀的 0.55 灰。正常光照下差异只在太阳高光上，因为环境光/IBL 还没做（步骤 9.2）。

## 边界与未做

- **没有 IBL，视觉收益被压住**。镜面反射需要环境，当前只有太阳直射光；接线的正确性要等 9.2 的 IBL 才能完整验收。
- **AO 未接**。R 全 0；且本工程把 occlusion 乘在直接光上，接真 AO 前要先改语义。
- **ORM 不压缩**。111 张 2048² 的 RGBA8 线性图（含 mip 每张约 22MB）合计约 2.3GiB 显存，是这次接线的主要成本。后续可考虑 BC1_UNORM（G/B 只剩 5-6 bit，粗糙度可能起色带）或 BC7。
- **Bistro 两份还没跑过 alpha 修正**（dry-run 报 19 / 11 处）。其中 `Paris_Table_cloth_01` 是直方图规则的假阳性：贴图 alpha 全 0 但材质是 OPAQUE（alpha 本来就被忽略），改成 MASK 会让桌布整块消失。不要在 Bistro 上盲跑 `fix_gltf_alpha.py`。
- **`KHR_materials_specular` 在当前渲染器被忽略**，删掉它不影响 ash_renderer 的渲染路径；保留方（只有 factor 的材质）将来接 IBL 时再决定怎么处理 F0 缩放。
- **Sponza 不受影响**：它的 glTF 自带标准 ORM 接线（同一张图挂 MR + occlusion），这套约定只适用于 ORCA 系 FBX 转换产物。

## 来源

- 通道约定：`assets/external/orca/EmeraldSquare_v4_1/README.txt`、`assets/external/orca/Bistro_v5_2/README.txt`
- glTF 通道语义：`ash_renderer/shaders/common/mesh.wgsl:60`（G=roughness、B=metallic）、`mesh.wgsl:70`（R=AO）、`ash_renderer/shaders/common/brdf.wgsl:122`（occlusion 乘直接光）
- 贴图格式决策：`ash_renderer/src/render/resident/assets/image_spec.rs`、`bcn.rs` 的 `TextureRole`
- 工具：`ash_renderer/tools/fbx_to_gltf/`（README 有完整用法）
- [KHR_materials_specular](https://github.com/KhronosGroup/glTF/tree/main/extensions/2.0/Khronos/KHR_materials_specular)：RGB=F0 颜色、A=specular strength，与本批数据的真实含义无关
- [NVIDIA ORCA / Emerald Square](https://developer.nvidia.com/orca)：源资产与参考渲染
