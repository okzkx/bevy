# Hi-Z遮挡流程：从渲染后深度到遮挡标记的各级账面

> 面向下一个读代码的人：6.1 引入了**第四条 GPU 侧数据链**（深度金字塔）与剔除链的第二刀。本文从"一帧里 Hi-Z 数据的生与死"走读整条链，配新文件分工表与代码改动实数；判定线证据与钉子归[施工记录](6.1.1-6.1.6-Hi-Z遮挡施工记录：测试实体载具、深度金字塔、遮挡第二pass与A_B计量.md)。

## 一、新文件分工表（一层一句话）

| 文件 | 一句话 |
|---|---|
| `shaders/hiz_downsample.wgsl` | 归约单入口：2×2 取 MIN 写下一级（reverse-Z 方向钉在注释里） |
| `shaders/occlusion.wgsl` | 遮挡判定：8 角点投屏 → 选 mip → 4 tap MIN → `obj_max < occluder` 判剔除，合并写 b3、独立写 b6 |
| `src/vulkan/hiz.rs` | HizPyramid：×2 帧槽 R32F mip 链本体 + 归约管线 + 遮挡测试描述符 + 读回通道 + reverse-Z 纯函数三件（单测） |
| `src/vulkan/occlusion.rs` | OcclusionPass：遮挡 compute 管线 + 80B push 打包 + 投屏参照 + OcclusionSettings/Diagnostics |
| `src/scene/mechanism/generation/test_entities.rs` | 测试实体本体：五形状 + 颜色 → 按形状/颜色缓存的 mesh/材质（遮挡样本载具） |
| `src/remote/shapes.rs` | `ash_renderer/spawn_shape` 协议薄壳（本体在 mechanism 层） |

改动沉淀的既有文件（要点）：`depth.rs`（usage 扩 SAMPLED + 能力查询）、`frames.rs`（storeOp STORE + 深度屏障 + 遮挡/归约录制位 + depth_image 访问器）、`instance_table.rs`（遮挡标记环第六族 b6）、`perf.rs`（4→5 位 timestamp、`gpu_hiz_ms`）、`driver/host/frame.rs`（遮挡半边编排 + OcclusionGate + resize 同批重建 + warmup 走账）、`driver/host/init.rs`（hiz/occlusion_pass 进创建链与拆除链——**拆除序：OcclusionPass → HizPyramid → FramePool**，视图先于深度 image 销毁）、`remote/cull.rs`（occlusion_set_enabled/hiz_census/occlusion_probe 三方法 + cull_stats 遮挡分列）、`overlay/windows/debug_window.rs` + `overlay/framework/ui.rs`（遮挡开关与状态行）。

## 二、一帧里 Hi-Z 数据的生与死（逐时刻走读）

帧槽 s = current，两帧前的同槽记为 s₋₂：

1. **帧首（CPU，`wait_for_slot` 之后）**：host 读账本组 DrawList、写双环；`OcclusionGate::push` 比较本帧相机 vs 两帧前（与滞后窗同对齐）→ 得出"遮挡本帧开不开"。
2. **视锥剔除 dispatch**：cull.wgsl 写 b3 = 视锥标记（5.2 原样）。
3. **遮挡测试 dispatch（新）**：occlusion.wgsl 读对象表 + b3 + **本帧槽金字塔（s 槽，内容 = 两帧前 s 槽归约输出——s 槽 fence 已过 = 写入提交完成，fence 是唯一执法）**，逐槽算保守遮挡，写 b6 = 遮挡判定、b3 = 视锥 ∧ 遮挡；自带屏障收口给 build_indirect。fallback/关闭/空场 = 整段跳过（b3 保持视锥纯结果，b6 host 填全可见）。
4. **build_indirect → 一次 indirect 绘制**：消费 b3（不变，5.3 形状零触碰）。
5. **渲染写深度**：dynamic rendering，深度 storeOp **STORE**（本帧深度帧尾要被采样）。
6. **深度屏障（帧尾，新）**：DEPTH_ATTACHMENT_OPTIMAL → DEPTH_READ_ONLY_OPTIMAL，LATE_FRAGMENT_TESTS → COMPUTE_SHADER。
7. **归约链（帧尾，新）**：`HizPyramid::record_reduction`——首次派发前插 UNDEFINED→GENERAL（或 GENERAL 内存屏障收口"遮挡读旧→归约写新"），然后 level 1..N 逐级：绑 (s, L) 描述符 → dispatch（2×2 MIN）→ 级间屏障。**level 0 不派发**（施工订正，见施工记录钉子 1）；level 1 的源 = 本帧深度（描述符声明 DEPTH_READ_ONLY），L≥2 的源 = 上一级（GENERAL）。
8. **帧计时 T3/T4**：归约区间与全帧区间采样（5 位 timestamp）。
9. **下帧**：深度 UNDEFINED 进场照常丢弃——金字塔才是持久层；遮挡读的又是"两帧前"的金字塔，如此轮转。

## 三、各级账面全景

```
深度附件（D32 ×2 帧槽）          —— 每帧清 0 重写、帧尾只读采样、下帧即弃
  └─ 归约链（compute ×11 级）    —— level 1..N 逐级 MIN → R32F 金字塔（×2 帧槽）
                                      │ 恒 GENERAL 布局； TRANSFER_SRC 供读回
                                      ▼
遮挡测试 dispatch（读 s 槽全链）  —— fence 保证读两帧前的确定值（零新同步形状）
        写 b6（遮挡独立标记，HOST_VISIBLE ×2 帧槽）—— A/B 归因读数口
        写 b3（= 视锥 ∧ 遮挡）  —— build_indirect 消费形状不变
```

- **金字塔生命周期** = resize 级（随 swapchain `rebuild`，device_wait_idle + pool reset + 描述符重写 + warmup 重置）；**内容生命周期** = 帧级（每帧帧尾全量重写，责任边界契约的"compute 链产物"半边，不经上传链）。
- **warmup 账**：`HizPyramid.warmup` 初值 2，每帧归约后递减；`ready()` 是遮挡 fallback 判据之一。重建后两帧内容未就绪，让路。
- **fallback 账**：`OcclusionDiagnostics`（enabled/dispatched/fallback 四态/遮挡判可见行数），只进计量。

## 四、失败与稳态路径

- **resize 途中**：闸门③帧首重建 swapchain → 深度 → 金字塔（同批、幂等）；重建失败 or_fatal 优雅退出（实抓过 pool 忘 reset 的 OUT_OF_POOL_MEMORY，已修）；重建成功 → warmup 2 帧 fallback → 自动恢复。
- **相机跳变**：OcclusionGate 两帧窗累计（位移 0.5 / 旋转 15°）→ 跳过 dispatch（b3 视锥纯结果、b6 全可见）→ 稳定两帧后自动恢复；跳变窗只有 2 帧，证据形态是 OcclusionGate 单测（轮询抓不到，施工记录已说明）。
- **空场**：bound=0 → 视锥 dispatch 与遮挡 dispatch 都 None，归约链照常录（深度=清屏 0，金字塔全 0——遮挡判据天然恒可见，安全）。
- **最小化**：整帧让路不碰 Vulkan，归约链与遮挡链都不经过（让路帧不 note_reduced——warmup 与实际录制的归约一一对应）。
- **拆 Device 排空**：teardown 序 OcclusionPass → HizPyramid → FramePool → …，hiz 自拆视图/image/内存/管线/布局/池；重建窗口期的半建状态由 rebuild 的 wait_idle 前置收口。

## 五、验证通道速查（BRP / brp.py）

| 命令 | 读数 |
|---|---|
| `brp.py cull-stats` | GPU 标记 vs CPU 视锥参照逐实体 + mismatches（=遮挡剔除清单）+ `occlusion` 节（开关/dispatched/fallback/遮挡判可见数） |
| `brp.py occlusion-set --enabled false\|true` | 数据层 A/B 开关（关 = b6 host 全可见 + 遮挡 dispatch 跳过） |
| `brp.py occlusion-probe [--entity E]` | 遮挡判定 CPU 参照：px 矩形 / mip 层 / 4 tap / occluder / obj 深度上界 / 判定，逐位对齐 shader |
| `brp.py hiz-census [--level N]` | 金字塔读回统计：逐级 min/max/mean/背景 0 占比（判定线 3 读数口） |
| `brp.py perf-stats` | `gpu_hiz_ms` 分位数（金字塔构建开销单列） |
| `brp.py spawn-shape <shape> --color R G B ...` | 遮挡对/墙体样本载具（五形状，按形状/颜色缓存零重传） |

## 六、代码改动实数（`git diff --stat a62fdaa16..1c1fd66d2`）

26 文件 +2752/−64，提交四笔：`5b482560c`（立案 docs）→ `4a759b5a2`（6.1.1 测试实体，+283）→ `f5afa0a90`（6.1.2~6.1.5 主体，+2303/−59，含 hiz.rs/occlusion.rs 两个新 vulkan 模块与两个新 shader）→ `1c1fd66d2`（OcclusionGate 单测，+38）。新增文件 6：两个 shader、hiz.rs、occlusion.rs、shapes.rs、test_entities.rs；扩展文件按家族归组：vulkan 侧（depth/frames/instance_table/perf/mod）、宿主编排（init/frame/mod）、读数与面板（remote/cull、remote/mod、remote/perf、overlay/ui、overlay/debug_window）、工具（tools/brp.py）。取舍要点：Hi-Z binding 走独立小布局不进 set2（冻结纪律优先）；遮挡标记独立 b6 + b3 合并（build_indirect 零触碰）；GpuTimer 4→5 位扩区间而非新建计时器。
