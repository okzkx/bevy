# GlobalProbe 方向桶：从捕获到着色求值

**一句话说明**：9.3 把"天空环境的光"从一条 SH-L2 系数扩成 16 条方向桶（每条 RGBL1），
投影与 9.2 的捕获脏标记同批重算，不透明与透明层按 `ibl_mode` 二选一求值；它是方向类别
LUT 与 9.7 的 Volume 外兜底格式，不是空间缓存、不含遮挡、默认仍是 SH-L2。

## 全景

```
天空参数/太阳/海拔
   │ sky_hash（9.2 既有）
   ▼
[脏 + 节流] ──► record_bake（② 段）
   │  sky_capture ─► mip0 cubemap（256²×6，GENERAL）
   │  sh_project / sh_reduce / sh_check（SH-L2 144B + stats）
   │  global_probe（9.3）：16 桶 × 32 半球样本 ─► probe buffer 1KB
   │  prefilter（mip1..8）
   ▼ 链尾屏障：compute 写 → 片元读 / cube 转 SHADER_READ_ONLY
帧 UBO ibl_mode ─────────────┐
                             ▼
不透明合成 / 透明前向（group 4）：b3 SH-L2 + b4 GlobalProbe 16 桶
   │  0 = sh_irradiance（9.2）
   │  1 = direction_to_bucket_index(n,4) → probe16_eval(l0,l1x,l1y,l1z)
   ▼
漫反射项 × ibl_intensity（高光仍走 9.2 预滤波 cube + DFG LUT）
```

BRP 读数/取证：`ibl_stats.probe16`（六方向 + 16 桶中心能量）、`ibl_census probe`
（64 float + 16 条中心方向）、`frame_dump`（LDR 帧 → PPM）、mode 11（桶可视化）。

## 文件分工（一层一句话）

| 文件 | 一句话 |
|---|---|
| `shaders/common/octahedral.wgsl`（新 41 行） | 球↔方映射、方向↔桶索引；9.5/9.7 共用同一份 |
| `shaders/scene/ibl/global_probe.wgsl`（新 70 行） | 16 桶 × 32 半球样本 → 折好常数的 RGBL1，写 1KB |
| `shaders/common/ibl.wgsl` | 加 `#import octahedral` 与 `probe16_eval`（值参数） |
| `shaders/scene/gbuffer/gbuffer_composite.wgsl` | 不透明漫反射按 `ibl_mode` 二选一；b4；mode 11 |
| `shaders/scene/transparent/transparent_forward.wgsl` | 透明层同款二选一（法线口径与不透明一致） |
| `src/render/passes/ibl/octahedral.rs`（新 153 行） | 上面映射/求值的 CPU 镜像 + 4 条单测 |
| `src/render/passes/ibl/environment.rs` | probe 缓冲/集/管线/初始化清零/烘焙链 dispatch/屏障/帧集 b4 |
| `src/render/passes/ibl/mod.rs` | `IBL_MODE_PROBE16`、octahedral 模块与常量出口 |
| `src/render/layout_registry.rs` | 新 set/管线登记进 `ALL_SETS`/`ALL_PIPELINES` |
| `src/render/resident/tables/frame_uniform_table.rs` | `FRAME_MODE_PROBE_BUCKETS = 11` |
| `src/render/frame/render_mode.rs` | `RenderMode::ProbeBuckets`（`probe_buckets`，ALL 12 项） |
| `src/render/core/target/color.rs` | ColorTarget 加 `TRANSFER_SRC`（帧取证用） |
| `src/render/passes/post/smaa.rs` | `readback_ldr`：LDR 后链目标拷贝回宿主 |
| `src/remote/features/ibl.rs` | `ibl_set.diffuse_mode`、`ibl_stats.probe16`、`ibl_census.probe` |
| `src/remote/census/frame_dump.rs`（新 66 行） | `frame_dump` BRP：写 PPM（锁屏兜底取证） |

## 逐时刻走读

1. **启动/建资源**：`IblEnvironment::new` 建 1KB probe 缓冲、`IBL_GLOBAL_PROBE_SET_DESC`
   （mip0 cube + 采样器 + probe buffer）、`GLOBAL_PROBE_PIPELINE`；内部池按 15 个集备容量；
   帧集 b4 绑同一个缓冲。`IC` 默认 `ibl_mode=0`（SH-L2）。
2. **首帧初始化**（无论 IBL 开关）：SH/stats/probe 三个缓冲一并 `fill_buffer 0`，然后一次
   内存屏障让后续 compute/片元读到确定零值。
3. **天空参数变化**：`draw_frame` 里 `sky_hash` 变化且过 0.1s 节流 → `prepare` 给出
   `capture=true`；`record_bake` 依次录 capture → SH 链 → **global_probe（1 次 dispatch，
   16 线程）** → prefilter，每步带交班屏障。
4. **投影内部**：线程 `bucket=gid.x` 取桶中心方向 → 32 个分层中点半球样本 → 累 `ΣL` 与
   `ΣL·ω` → 乘 `(2π/N)·A·Y²` → 写 `probe[bucket*4 .. +3]`。
5. **着色**：不透明合成与透明前向都先用 9.2.8 的 `ibl_normal` 取视向侧法线；`ibl_mode==0`
   走 `sh_irradiance`，`==1` 走 `direction_to_bucket_index(n,4)*4` + `probe16_eval`；
   漫反射乘 `Fd_Lambert · ibl_intensity`。高光两条分支完全一致。
6. **调试/读数**：mode 9/10 自动跟随 `ibl_mode`；mode 11 输出桶中心颜色；`ibl_stats` 读
   SH + stats + probe 三个缓冲（各自一次性拷贝），算六方向与桶中心能量；`ibl_census
   probe` 回全量；`frame_dump` 读 LDR 后链目标写 PPM。

## 稳态与失败路径

- **稳态**：天空/太阳/海拔不变 → `sky_hash` 不变 → 不录任何烘焙命令，合成/透明只读
  SH 与 probe 两份常驻缓冲；零额外 GPU 成本。
- **IBL 关**（默认）：帧 UBO `ibl_enabled=0`，合成/透明回 `albedo × ambient`；probe 停留在
  初始化零值，不参与求值。
- **天空关**：9.2.8 起 `ibl_enabled = ibl.enabled && sky.enabled`，环境项回常量 ambient；
  probe 不重烘（捕获门控同样关闭）。
- **首次捕获前**：probe 全零，`ibl_stats` 的 `capture.count=0`、`probe16` 能量为 0；
  `ibl_mode=1` 的漫反射为 0（不是未定义值）。
- **`frame_dump` 在首帧前**：LDR 仍为 UNDEFINED，读回会触发验证错误——诊断口约定，
  BRP 调用方在宿主跑起来之后再取。

## 代码改动简要解析

`git diff --stat`（相对 `18b805c4d` 施工起点）：14 个修改文件 +381/−51，另有 4 个新文件
330 行。

**新增**

- `shaders/common/octahedral.wgsl`（41）：映射与桶索引的 GPU 正本；自带 clamp，不依赖 brdf。
- `shaders/scene/ibl/global_probe.wgsl`（70）：16 桶投影；权重把 `(2π/N)·A·Y²` 一次折完，
  采样用分层中点而不是 `i/N`（后者常量天空会低 1.6%）。
- `src/render/passes/ibl/octahedral.rs`（153）：CPU 镜像与 4 条单测；桶常量复用 environment.rs。
- `src/remote/census/frame_dump.rs`（66）：锁屏环境下的帧取证口，PPM P6。

**扩展（按层）**

- 渲染资源/录制：`environment.rs`（probe 资源、初始化清零、烘焙链插入、屏障、帧集 b4、
  `probe_buffer()` 访问器、池容量 +1）；`layout_registry.rs`（契约登记）。
- 着色：`ibl.wgsl`（`probe16_eval`）、`gbuffer_composite.wgsl`（b4/分支/mode 11）、
  `transparent_forward.wgsl`（b4/分支）。
- 宿主形态：`render_mode.rs` + `frame_uniform_table.rs`（mode 11）、`render/mod.rs`（出口）。
- BRP：`remote/features/ibl.rs`（`diffuse_mode`/摘要/全量）、`remote/mod.rs`（注册与文档）、
  `remote/census/mod.rs`（模块声明）。
- 取证基建：`core/target/color.rs`（TRANSFER_SRC）、`passes/post/smaa.rs`（`readback_ldr`）。

**取舍**

- 用"一桶一线程串行 32 样本"替掉 Unity 的 groupshared 归约：512 次采样成本可忽略，换来
  不引入 workgroup 内存与组内同步。
- 存储取 4×vec4（64B/桶、1KB），比 Unity 的 48B/桶多 256B，换来读写不用手工拆包。
- 默认仍 SH-L2：A/B 无量化收益也无伪影，16 桶留作 9.7 兜底（与 patch 同格式）。
