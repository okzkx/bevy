# GPULOD流程：从阶梯样本到按档命令的各级账面

> 6.2 功能流程导读（2026-10-19 收官定例）：本段引入了新数据流（LOD 组登记 → 档位资产驻留 → GPU 选档 → 按档命令），按定例写"从入口动作到最终落点"的走读。面向下一个读代码的人；验收证据归[施工记录](6.2.1-6.2.6-GPULOD施工记录：场景脚本化、LOD变体表与选档联动.md)。

## 0. 一句话全景

BRP `spawn_lod` 造一个四档阶梯实体并登记 `LodGroups`；上传链把档位资产一并驻留进 MeshPool；帧循环对换槽行把"槽 → 各档绘制引脚"写进 LOD 变体表环（b7），选档 compute 按屏幕覆盖尺寸把档位写进档位环（b8），build_indirect 按档从 b7 取引脚组装命令——绘制三字段（indexCount/firstIndex/vertexOffset）随距离变粗，instanceCount 语义不变。

## 1. 新文件分工表

| 文件 | 一层职责 |
|---|---|
| [scene/mechanism/lod_registry.rs](../../../../ash_renderer/src/scene/mechanism/lod_registry.rs) | LOD 组登记处：基准身份→各档变体柄（flush 盘点/表写/census 共用）+ 阶梯 spawn 本体（形状 × 档位缓存） |
| [vulkan/lod.rs](../../../../ash_renderer/src/vulkan/lod.rs) | 选档 compute 半边：LodPass 管线 + 96B push + 判据/迟滞纯函数（CPU 参照）+ 开关/诊断资源 |
| [shaders/lod_select.wgsl](../../../../ash_renderer/shaders/lod_select.wgsl) | 选档 shader：8 角点投屏 → floor 分带 + 迟滞 → 写 b8 |
| [remote/lod.rs](../../../../ash_renderer/src/remote/lod.rs) | BRP 方法族薄壳：lod_census（逐实例对照）/lod_set_enabled/lod_set_params |

扩展件：instance_table.rs（两条新环 + set2 b7/b8）、ledger.rs（LOD 脏集两套奇偶）、upload.rs（档位盘点/换槽置脏/到货反查三挂点）、drawlist.rs（write_lod_table_ring）、build_indirect.wgsl（pick_lod_entry 按档取引脚）、frames.rs（FrameDraw.lod + 录制位）、frame.rs（LOD 半边：dispatch/关闭填充/诊断）、shapes.rs（spawn_lod 薄壳）、tools/scene_lod.py（场景脚本持久化）。

## 2. 逐时刻走读

**入口（BRP spawn_lod，帧节拍）**：薄壳解析参数 → `spawn_lod_ladder`——基准档走 `spawn_test_shape`（形状/材质缓存命中共享柄）→ level 1.. 变体网格查 `LodGroups.variants` 缓存、查无构造 `Sphere::ico(3/2/1)` 入 `Assets<Mesh>` → `register` 登记映射（by_base + level_to_base）。实体带 Mesh3d 走账本 Added 通道自动入行。

**上传链（flush_uploads，Last，先于帧循环）**：脏行盘点时，行 mesh 在登记组内 → **非基准档**未驻留的补进本批（去重 lod_seen；失败档跳过 = 哨兵降级）；mesh 腿完成 → 换槽写对象记录（staging 批）+ `mark_model_dirty` + **`mark_lod_dirty`**（两套奇偶集）。批提交成功 → 档位资产 commit → 反查 `base_of` 重挂引用行 LOD 脏标（档位晚到也刷新表）。

**帧循环（draw_frame，wait_for_slot 之后）**：
1. `write_rings` → `write_lod_table_ring`：消费本帧奇偶 LOD 脏集——level 0 = 本行基准 mesh 槽位引脚、level 1.. = 登记组变体引脚（未驻留 = 哨兵 0），48B 写进本帧槽 b7 环的槽直址处；同槽 b8 写 0（迟滞复位）。
2. **LOD 半边**：开关开 ∧ bound>0 → `LodPass::draw`（96B push：view_proj/screen/bound/threshold/hysteresis）进 FrameDraw.lod；关 → host 把 b8 环 bound 段填 0（最细档，命令回 5.3 基准）。诊断：b8 直读逐在场行统计档位分布。

**命令录制（record_frame，compute 三连）**：视锥剔除 → 遮挡测试 → **LOD 选档**（读 b0/b1 投屏 + b8 迟滞 prev，写 b8 本帧档位；屏障 compute 写→读）→ build_indirect（读在场环/标记/对象表 + **b7 按档取引脚** + b8 档位）→ 一次 `cmd_draw_indexed_indirect`。图形 VS/FS 零改动（引脚在命令里，VS 按 firstInstance 槽号读 set2 的 b1/b2 不变）。

**失败与稳态路径**：非阶梯实体 = b7 全哨兵 → `pick_lod_entry` 回退对象记录基准引脚（5.3 行为）；档位资产未驻留 = 该档哨兵 → 向细档回退；全缺（理论不可达）= instanceCount 防御归 0；LOD 关 = b8 host 填 0；静态场景稳态 = LOD 脏集空，零盘点零表写（LOD 纯 compute 每帧重算档位，无增量问题）。

## 3. 判据与迟滞语义（读 shader 前先读这段）

- 自然档位 `raw = floor(log2(threshold / s))`，s = AABB 投屏 **clamp 进屏后**矩形最大边（离屏 → 0 → 最粗档，反正不画；w≤0 → 恒 0 最细档，与遮挡"恒可见"同方向）。
- 迟滞：向粗（raw > prev）需 `s ≤ B(raw)×(1−h)`（越过阈值带才降档），向细（raw < prev）即时。prev = b8 本槽值 = **两帧前**本帧槽输出（fence 纪律），滞后由迟滞带吸收。
- 默认参数 threshold=256px、hysteresis=0.25（LodSettings，BRP/egui 可调，随 96B push 每帧进 compute）。

## 4. 代码改动简要解析（git show --stat 实数）

```
已跟踪 18 文件 +564/−54（git diff --stat 3f615068d 实数），新文件五件：
 build_indirect.wgsl            | 59 +++++++--（按档取引脚 + 哨兵回退）
 instance_table.rs              | 147 +++++++---（两条新环 + set2 b7/b8）
 lod_select.wgsl                | 110 行（新）
 vulkan/lod.rs                  | 379 行（新：LodPass + 纯函数 + 开关/诊断）
 remote/lod.rs                  | 228 行（新：BRP 三方法）
 scene/mechanism/lod_registry.rs| 203 行（新：登记处 + 阶梯 spawn）
 drawlist/frame/upload/init     | LOD 半边接线（64/77/52/19 行级）
 ledger/shapes/scene mods       | LOD 脏集 + 薄壳 + 导出
 overlay ui/debug_window        | LOD 面板行（8/19 行）
 tools/brp.py +52 / scene_lod.py 277 行（新）/ README 登记
```

取舍要点：档位传递走**独立环**（b8）而非视锥标记位打包——A/B 归因干净；LOD 表走 **HOST_VISIBLE 增量环**而非 DEVICE_LOCAL 写一次——档位资产可能晚到（哨兵→引脚刷新），增量环天然吸收，且宿主 census 免费；选档**每帧现算**不做跨帧 fallback 专段——判据是相机纯函数，无状态可失效。
