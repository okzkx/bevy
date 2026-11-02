# 施工计划：8.2.6 spawn 增量链缺陷修复（材质记录全零与运行期 spawn 无片元）

> 2026-10-22 立案（用户指令"新增 8.2.x 小步骤对 GBuffer 测试简单几何场景时暴露出来的问题进行修复"，纯文档立案）。对应[段施工计划 §1](施工计划：Filament%20BRDF%20与%20多光源光照累积.md)与[段 README](README.md)：修复 8.2.5 G-buffer 调试仪器在简单几何场景验收轮照出的两条增量链缺陷；本文管 8.2.6 做什么、按什么顺序、验收什么；"实际做了什么"开工后另立施工记录，不回写本文。
>
> **当前状态：⬜ 已立案（2026-10-22，待施工）**。施工顺序：先于 8.2.5 总验收（缺陷不清，总验收的梯度读图与既有判定线零回退两项会被污染）。

**一句话说明**：本任务修复两条运行期增量链缺陷——运行期 spawn 的行部分不产生片元（缺陷②）与部分材质记录全零（缺陷①），把"画面异常先切仪器定位"的取证能力落成"定责可修"；本任务不主张任何新渲染功能与画面语义（不碰 BRDF/光照/pass 结构），修完以简单几何场景九通道读图为验收，为 8.2.5 总验收与后续段清场。

## 0. 背景与证据链（定责输入）

8.2.5 验收轮（2026-10-22，cube.gltf 场景 + spawn-shape 球×6 + mutate + 点光，像素级定量判读，证据 `.temp/8.2-gbuffer-sphere/`）实抓：

- **G-buffer 写入与九模式仪器本体无责**：glTF 立方体九通道逐项精确（albedo 0.799/normal 编码/metallic 0/roughness 0.402/ao 1/reflectance 0.497/depth 合理）；球E（slot 5）基线九通道全对 + mutate 写后读闭环过（metallic 0→1.0 精确、roughness 0.5→0.098 含 8bit 量化、albedo 不动）。
- **缺陷②（spawn 球无片元，zero_day 轮已录）受控复现**：slot 1/3/4 三球零像素。铁证=set_transform 挪进画面正前后截图 md5 一字不变（模型环写对受影响行也不生效，或对象记录本身退化）；cull GPU 标记却全可见（退化零 AABB 在原点过视锥的自洽签名）；zero_day 轮 position 模式球心像素反解出墙的世界坐标（NaN 级裁剪坐标症状）。
- **缺陷①（primitive 材质记录全零，zero_day 轮已录）受控复现**：slot 2 球对象行正常（位置像素级精确）但材质读回垃圾——albedo=0/metallic=0/ao=0/reflectance=0/roughness=0.0437，与 zero_day 一批 primitive 的 0.045 同签名（未写槽）。材质环每帧全量重写（`write_material_ring`，drawlist.rs:218）语义下"垃圾"只能是**槽号错位或集合缺员**，不可能"陈旧"。
- **共性定性**：同一 spawn-shape 路径两缺陷面同场并存，且同批内 slot 5 完全正常——**行批次相关、非路径必然**；`drawlist verify` 零 mismatch 只对账 CPU 两侧组装（增量 vs 全量参照都在 CPU），不证明 GPU 光栅化，现有仪器面存在"GPU 表内容"盲区。
- **嫌疑面**（立案期代码核实，均属实）：①对象记录经 staging 批次写入（upload.rs:403-440，`staged_records`），批次提交路径可能丢行/错位；②模型环两帧奇偶集（ledger.rs:283-291 `mark_model_dirty` 进两套、`take_model_dirty` 按帧取走），与批次交错时可能漏写；③材质环全量重写以 `assembly.materials`（可绘行去重材质）为宇宙，`material_slot` 查无即防御性跳过——可绘集判定与对象记录 mat_index 的时序不一致可产生错位。

## 1. 任务拆分

| 任务 | 交付 | 状态 |
|---|---|---|
| 8.2.6.1 定责与最小复现 | BRP 读回调试通道（材质环/模型环 HOST_VISIBLE 可直读字节；对象表 DEVICE_LOCAL 需 staging 读回——按 BRP 优先定例扩自定义方法族，命名施工时钉明）+ 固定复现脚本（tools/ 下，cube 场景 + 固定 spawn 序列，一键复现两缺陷；命名施工时钉明）+ 定责结论（丢失点锁到行级：staged_records 提交 / 环写 / 集合缺员三选一或并存） | ⬜ |
| 8.2.6.2 修复 | 按定责结论修复（预期落点：upload.rs flush 批次提交 / drawlist.rs 环写 / ledger 组装判定时序，以实测定）；修复不引入每帧全量上传（增量链语义红线）；零 VUID | ⬜ |
| 8.2.6.3 判定线回归 | 回归脚本全绿 + 判定线逐条（见 §2）+ 既有判定线零回退（库测试/clippy/exit 0/GPU-driven 口径） | ⬜ |

## 2. 判定线草案

1. **简单几何场景全渲染**：cube + spawn-shape 六球（含 mutate 过的）全部可见，九通道读图逐项对期望值（截图+像素采样定量，8.2.5 验收轮同款方法），无缺员无全零材质。
2. **set_transform 写后读**：任意 spawn 实体移动后画面随动（position 模式反解世界坐标与新 Transform 吻合，非仅 md5 变化）。
3. **mutate 写后读不回退**：metallic/roughness 热改次帧生效（8.2.4 既有面，metallic 精确、roughness 8bit 量化容差）。
4. **zero_day 抽查**：缺陷①签名消失——AO 模式无"全黑块"、四通道交叉无 albedo=0+roughness≈0.045 组合（若加载时长不可接受，回退 cube 场景高频 spawn 压力序列替代，施工时钉明）。
5. **既有判定线零回退**：库测试全过、clippy 全净、零 VUID、WM_CLOSE exit 0、5.3 draw_calls 口径不回退、hiz/lod/stream/fine 四脚本回归绿。

## 3. 测试方案

- **BRP 优先**：定责读回走自定义方法族扩列（brp.py 子命令化），不写一次性探针；复现/回归脚本持久化 tools/（hiz_test.py 先例：一键入口+--no-close/--skip-launch 旗标）。
- **画面验证**：PrintWindow 截图 + 当前会话模型读图 + 像素采样定量（画面功能视觉验证规范）；**截图前窗口整体挪进屏内**（8.2.5 大钉：DWM 对屏外区域不重定向）。
- **排查方法论钉（8.2.5 轮实抓）**：直出诊断采样避开 egui 面板区域；PrintWindow 可能返回陈旧同 md5 帧，验活用 perf_stats 两次采样差分。

## 4. 边界

- **不主张新功能画面**：BRDF/光照语义、pass 结构、G-buffer 布局零变化；本任务是纯缺陷修复。
- **8.2.5 总验收仍单列**：本任务判定线只覆盖缺陷修复面，不替代 8.2.5 的梯度读图/DamagedHelmet 对照/性能成组记录。
- **整树 despawn device lost（段 README 已录严重项）不在本任务定责**：若定责发现两缺陷与回收链同源，只修本缺陷面并在施工记录钉明关联，device lost 独立立案。
- **增量链语义红线**：修复不引入稳态每帧全量上传、不把 GPU-driven 决策搬回 CPU（draw_calls 维持 5.3 口径）。
