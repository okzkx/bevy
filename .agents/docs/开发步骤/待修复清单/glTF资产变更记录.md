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

施工脚本 `fix_gltf_look.py` 两个子命令各跑一遍，随后二次运行确认幂等。

### JSON 记账

| 文件 | 删除占位 primitive | 二次运行 | 玻璃改 BLEND | 二次运行 | 材质总数 |
|---|---|---|---|---|---|
| EmeraldSquare_Day.gltf | 3 | 0 | 9 | 0（已是 BLEND 9） | 220 |
| EmeraldSquare_Dusk.gltf | 3 | 0 | 9 | 0（已是 BLEND 9） | 222 |

- 两文件 `WorldGrid*` primitive 均为 0；材质表 `WorldGridMaterial*` 条目各 3 个保留。
- 9 个玻璃材质均为 `alphaMode=BLEND`、`baseColorFactor=[1,1,1,A]`（A 见上表）；`Emissive_Window*` 未改。
- 与备份逐字段对照：除上述 primitive 与玻璃材质字段外无差异；写回保持原 LF 行尾、无 BOM。
- 备份：`.temp/gltf_look_backup/EmeraldSquare_Day.gltf`（原 md5 `7c9946b67ad4cbbbad76298e48ef875c`）、`.temp/gltf_look_backup/EmeraldSquare_Dusk.gltf`（原 md5 `1cca71189c1e88aacc55a2660edfef31`）。

### 运行期（Emerald Day）

- `ash_renderer/ring_census`：2476 条实例行、217 个被引用材质槽；材质级 `alpha_mode` 分布 `OPAQUE/0=179`、`MASK/1=29`、`BLEND/2=9`；实例行分布 `0=1492`、`1=964`、`2=20`。9 个玻璃材质名全部命中 `alpha_mode=2`，`material_all_match=true`。
- `ash_renderer/transparent_sort_check`：`transparent_rows_cpu=20`、`sorted=true`、`issues=[]`。
- 日志（`.temp/taskA_run.log` / `.temp/taskA_run_err.log`）：`VUID` 0、`Validation Error` 0、panic 0。
- 启动耗时：进场资产加载约 1 分 45 秒后 BRP 才应答，长于 60 秒等待窗口。

### 截图与读图

- 机位 1（原视角，看墙根）：改前同机位取证图 `.temp/restored.png`，改后 `.temp/taskA_cam1.png`；局部对照 `.temp/taskA_cam1_before_crop.png` / `.temp/taskA_cam1_after_crop.png`。墙根暗棋盘条带消失，同位置由 `Floor` 灰面兜底；右侧店面玻璃可透见室内阴影与框线，不再是纯黑面。
- 机位 2（公交/窗户视角）：`.temp/taskA_cam2.png`。公交窗内仍是既有的近级阴影网纹（F29/F30，本批不处理），未见占位棋盘残留或蓝洞/黑洞。
- 机位 3（侧面）：`.temp/taskA_cam3.png`。整体几何无破洞，透明层无排序错乱。

## 观感决定（2026-11-23）

用户拍板 **保持现状，等 9.2 IBL 后再复评玻璃**（选项 C）：

- 大楼玻璃现为 BLEND（A=0.35~0.5），但底色近黑（(44,45,44) → 线性约 0.025）且建筑是纯外壳（无室内几何），透过去是黑的，观感偏"深色镜面"；等 IBL 落地后深色玻璃反射天空才是正确形态，届时再决定是否补假室内或调底色。
- 公交窗不受玻璃材质控制：整车只有 `Bus_Etc`（12725 顶点，含窗面/灯/格栅）与 `Bus_Body`（380 顶点大面）两个不透明材质，窗面是 `Bus_Etc` 的贴图/几何一部分；不拆资产就无法单独透明，本次不做。
- 若以后要改：按 A（提亮玻璃底色 + 降 alpha + 抬 roughness）、B（补假室内）、D（拆公交窗几何）另立任务，不改本次已经落地的 BLEND 结构。

## 回滚

```powershell
# 例：恢复 Day 场景
Copy-Item .temp/gltf_look_backup/EmeraldSquare_Day.gltf assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf -Force
```
