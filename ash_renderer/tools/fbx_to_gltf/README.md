# fbx_to_gltf

ORCA 系（Emerald Square / Bistro）FBX 资产转 glTF 的完整工具链。

## 三步流程

| 步骤 | 谁做 | 干什么 |
|---|---|---|
| 1. 转换 | `blender_convert.py`（在 Blender 内跑） | 导入 FBX，按清单改 metallic/alpha 接线，导出 GLTF_SEPARATE |
| 2. ORM 接线 | `fix_gltf_orm.py` | 把错挂到 `KHR_materials_specular` 的 ORM 图改接 `metallicRoughnessTexture` |
| 3. alpha 语义 | `../../../tools/fix_gltf_alpha.py` | 按贴图 alpha 直方图裁决 MASK/OPAQUE（沿用现有工具） |

第 2、3 步由 `convert` 自动串起来，也可以对已有资产单独补跑。

## 用法

```bash
# 完整转换（仓库根目录执行）
python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py convert \
    assets/external/orca/EmeraldSquare_v4_1/EmeraldSquare_Day.fbx \
    assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf

# 只补 ORM 接线（先看改动再落盘）
python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py fix-orm --dry-run \
    assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf
```

Blender 查找顺序：`--blender` 参数 → 环境变量 `BLENDER` → PATH → `C:/Program Files/Blender Foundation/Blender */blender.exe`。

## ORM 接线的规则与禁区

源资产的 Specular 贴图是 **R=Occlusion、G=Roughness、B=Metallic**，与 glTF 的 ORM 打包一致。
转换脚本要做的是把它接到 `metallicRoughnessTexture`，并把 `metallicFactor`/`roughnessFactor` 置 1.0，
让数值全部由贴图决定。

- **不接 `occlusionTexture`**：这批资产的 R 通道全 0（没烘 AO），接上等于让 AO=0，
  渲染器又拿 occlusion 乘直接光，画面会整体变黑。
- **删掉 `KHR_materials_specular`**：Blender 把 ORM 图的 RGB 当成 F0 颜色写进该扩展，
  属于误读；A 通道在原图里全是 255，删除不丢信息。需要保留时加 `--keep-specular`。
- **只动带 `specularTexture` 的材质**：没有贴图、只有 `specularColorFactor` 的材质
  （水、冰、酒、自发光窗）保持原样。

写盘前会把原文件备份到 `.temp/gltf_orm_backup/`（已有备份不覆盖）。JSON 按 Blender 导出风格
（tab 缩进、冒号后无空格）写回，保留原行尾。

## 依赖

- Blender 4.x / 5.x（自带 glTF 导出器）
- Python 3（第 3 步需要 Pillow）
