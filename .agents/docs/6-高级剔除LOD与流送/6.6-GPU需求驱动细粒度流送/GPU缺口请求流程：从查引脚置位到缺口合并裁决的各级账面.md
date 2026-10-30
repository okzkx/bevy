# GPU 缺口请求流程：从查引脚置位到缺口合并裁决的各级账面

> 6.6 功能流程导读（2026-10-20 收官，面向下一个读代码的人）：GPU 的"命中且未驻留"差集如何变成稀疏缺口清单、跨帧异步读回、并进 CPU 的需求裁决——全程无当帧等待、无 ACK/重试协议。验收证据归[施工记录](6.6.1-6.6.6-GPU缺口请求施工记录：缺口标记环、快照采样与CPU合并裁决.md)，责任分界背景见[6.4 CPU/GPU 交互篇](../6.4-细粒度流送/粗细粒度流送的CPU与GPU交互：现状与稀疏请求.md)。

## 0. 一图看懂：缺口从产生到被消费

```text
cull compute（dispatch = 槽 bump bound）
  └─ gap_marks[slot] = 0                    ← 清零段：全部槽每帧归零
build_indirect compute（dispatch = 在场行数）
  └─ 选中档条目哨兵? → gap_marks[slot] = 1<<level   ← 缺口置位（差集现成点）
        （回退绘制照旧——缺口只记录不改变画什么）
帧起（draw_frame，fence 纪律内）
  └─ GapSnapshot.bits = 本帧槽缺口环快照      ← 两帧前输出，采样收口
residency_tick（Last）
  ├─ 逐槽位图 → 账本反查行 → LOD 组 → 基准身份聚合   ← 槽级去重 + 组级归并
  ├─ 逐档复核驻留（延迟内可能已到货）→ 仍缺才请求
  ├─ 缺口请求：豁免半径、保留冷却窗 → 引用行置脏       ← GPU 出意见
  └─ merged 需求 = CPU 预测 ∪ 缺口 → LodDemand 位图   ← CPU 拍板
flush_uploads（同帧）
  └─ 盘点按 merged 位图过滤 → 缺口档位获得上传通道 → 驻留更新
下一帧 b7 表哨兵变化 → GPU 自然选到高档 → 缺口自愈归零
```

## 1. 新资源与文件分工

| 项 | 位置 | 一句话职责 |
|---|---|---|
| 缺口标记环（b9） | `vulkan/instance_table.rs` | HOST_VISIBLE ×2 帧奇偶，逐槽 u32 位图；hazard 表新增一行 |
| `gap_marks` 清零 | `shaders/cull.wgsl` | dispatch 覆盖全部槽，清零段 |
| `gap_marks` 置位 | `shaders/build_indirect.wgsl` | 选中档哨兵 → `1<<level`，回退绘制照旧 |
| `GapSnapshot` | `scene/mechanism/assets/residency.rs` | 帧起采样的缺口快照——tick/BRP 的统一消费口 |
| `gap_mode` 三态 | 同上（ResidencySettings） | Off/GpuOnly/Merged——A/B 开关 |
| `gap_asset_ids()` | 同上（pub 导出） | 位图→资产身份纯函数（越界位忽略） |
| merged 归并点 | residency_tick fine 块 | 需求位图与淘汰口径升级为 merged |
| BRP 三方法 | `remote/gap.rs` | gap_stats/gap_census/gap_set |
| `tools/scene_gap.py` | tools/ | 判定线十项一键压测 |

## 2. 逐时刻走读（scene_gap 场景为线）

1. **Off 基线**：缺口环照常被 cull 清零/build_indirect 写（GPU 侧不开关），tick 不消费（gap_mode=Off）——6.5 CPU 预测独走语义零变化。
2. **GpuOnly**：CPU 期望档预测请求关闭；基准被预算淘汰后，选中档哨兵 → 缺口置位 → 快照 → tick 聚合 → 行置脏 → merged 位图 → flush 上传 → 下一帧 b7 到货 → GPU 选到高档 → 缺口归零——**缺口→上传→自愈闭环**（判定线 1 的 rows_total=72 与 used 上升为证）。
3. **merged**：CPU 预测与缺口并集进请求集与位图——升级时机吃 GPU 一手证据（迟滞/遮挡后的真实选档），供给权不挪窝。
4. **off 哑火 → 回开**：off 期快照不消费（rows_last=0），缺口位图持续重写（GPU 不记得发过请求）；回开后缺口自然重新报出——"丢失只推迟升级"（判定线 3）。
5. **全常驻负例**：预算 0 → 无缺席 → 缺口恒零、零请求（判定线 4 的恒空面）。

## 3. 失败与稳态路径

- **陈旧缺口**：快照两帧延迟内的行清账/槽回收 → slot→row 反查失败 → 如实计数丢弃（gap_stale_total）；槽复用碰撞窗口 = 停尸房 2 帧纪律同窗口，误请求一次由冷却窗与幂等续期吸收（施工记录钉明）。
- **请求半径外的缺口**：豁免半径（已命中可见证据）；预算压力下仍会被 coarse 淘汰循环 shed——over_budget 如实报，缺口下帧重报。
- **稳态**：全驻留时缺口环全零、零盘点零上传（增量链语义不变）；缺口活跃期 CPU 帧 p95 与关闭档无差（快照采样零阻塞）。

## 4. 代码改动简要解析（`git show --stat` 实数）

**feat 2ff40d647**（10 文件，+426/−40）：
- `vulkan/instance_table.rs`：缺口环家族 + b9 描述符接线 + hazard 表行（照 b7/b8 扩法，解冻清单=布局加一条 binding）。
- `shaders/cull.wgsl` / `build_indirect.wgsl`：清零段与置位点——GPU 侧总共两条语句级改动。
- `scene/mechanism/assets/residency.rs`：GapMode/GapSnapshot/gap_asset_ids + tick 缺口聚合/merged 归并/三态闸门（本段逻辑主体）。
- `driver/host/frame.rs`：GapSnapshot 采样 + cull 关闭补零。
- `remote/gap.rs` + `remote/mod.rs`：BRP 三方法。
- `scene/{mechanism/,}mod.rs`、`vulkan/mod.rs`：导出。

**feat 7d49504d7**（1 文件，+310）：`tools/scene_gap.py`。

取舍要点：缺口是"建议"不是命令——进 merged 需求即被 CPU 拍板接受，但预算/冷却/淘汰裁决全部原位；缺口环零原子（每槽单写者），cull 清零 + build_indirect 置位的两段式让"每帧全量重写"纪律不用 host 参与（cull 开启时）。三态里 GpuOnly 的存在证明缺口足以独立驱动细档供给，但默认形态保守（Off）——升级由 BRP/场景脚本显式开。
