# glTF 资产变更记录（Emerald Day / Dusk）

> 资产目录 `assets/` 由本地 git exclude 排除，改动不进版本库；本文件是这些表现调整的唯一追溯。施工脚本 `ash_renderer/tools/fbx_to_gltf/fix_gltf_look.py`，可重复执行（幂等）。
>
> 关联计划：[渲染观感优化计划](渲染观感优化计划.md) 任务 A。

## 原则

- 只改场景表现参数与占位几何，不动转换管线的产物结构（材质、节点、贴图引用之外不改）。
- 每次写盘前备份到 `.temp/gltf_look_backup/<原文件名>`；已存在备份不覆盖（保留首次改前快照）。
- 回滚 = 用备份文件覆盖回原位。

## 变更 1：删除 WorldGrid 占位地面

源资产（Unreal 场景）残留的 `GroundPlane` / `GroundPlane2` / `GroundPlane3` 各带一个 5500×5500 的占位 quad（`WorldGridMaterial*`，棋盘占位贴图，法线朝下），与同 mesh 的 `Floor*` quad 完全同高共面。删除占位 primitive，保留 Floor 四边形兜底。

| 文件 | mesh（名称 / 序号） | 删除的 primitive | 材质 |
|---|---|---|---|
| EmeraldSquare_Day.gltf | Mesh.031 / 22 | primitive[1] | WorldGridMaterial |
| EmeraldSquare_Day.gltf | Mesh.083 / 101 | primitive[1] | WorldGridMaterial1 |
| EmeraldSquare_Day.gltf | Mesh.117 / 133 | primitive[1] | WorldGridMaterial2 |
| EmeraldSquare_Dusk.gltf | Mesh.043 / 43 | primitive[1] | WorldGridMaterial |
| EmeraldSquare_Dusk.gltf | Mesh.098 / 98 | primitive[1] | WorldGridMaterial1 |
| EmeraldSquare_Dusk.gltf | Mesh.130 / 130 | primitive[1] | WorldGridMaterial2 |

材质表里的 `WorldGridMaterial*` 条目保留（不再被任何 primitive 引用，无副作用；便于对照首跑快照）。

## 变更 2：玻璃材质改半透明

规则：名字命中下表且当前为 `OPAQUE` 的材质，改 `alphaMode: "BLEND"`，并在 `pbrMetallicRoughness.baseColorFactor` 里保留 RGB=1、把 A 设为下表的半透明值。`Emissive_Window*` 是自发光窗，不在此列（改透明会破坏发光语义）。

| 材质 | 半透明 A | 说明 |
|---|---|---|
| glass | 0.35 | 玻璃幕墙/公交候车亭 |
| hotel_windows | 0.40 | 酒店窗帘后窗 |
| Tower_Windows_Ball | 0.40 | 塔楼球窗 |
| Sheleg_Glass_Sign | 0.50 | 玻璃招牌 |
| Glass_Signs | 0.50 | 店面玻璃牌 |
| Generic_2_Glass_Roof_Awning_Red / Red1 / Red2 / Red3 | 0.45 | 红色玻璃雨棚 |

Day 命中 9 个材质，Dusk 命中 9 个（同名）；两场景的 `Emissive_Window*` 保持 OPAQUE。

## 验证记录

（施工后回填）

- JSON 记账：占位 primitive 0 处、玻璃 BLEND 材质数。
- 运行期：`ash_renderer/ring_census` 的 alpha 模式分布、`ash_renderer/transparent_sort_check` 透明行数、日志 VUID 计数。
- 截图（临时证据，不入库）：`.temp/` 下前后对比图路径与读图结论。

## 回滚

```powershell
# 例：恢复 Day 场景
Copy-Item .temp/gltf_look_backup/EmeraldSquare_Day.gltf assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf -Force
```
